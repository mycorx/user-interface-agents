// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! A stdio MCP test server (protocol 2026-07-28) for trying UIA's alert cards
//! by hand. It serves one alert feed, `uia-test://alerts`, and every
//! `ALERT_FEED_INTERVAL_SECS` seconds (default 15) appends the next entry of a
//! fixed sequence and tells listeners with `notifications/resources/updated`:
//! the T-10, T-5, T-2 and T-0 ladder for one fake meeting, an `invited` alert,
//! an alert without an action, and an entry without a `summary` (which UIA
//! must ignore). After the last entry it stays up and goes quiet; restart it
//! to replay. It also exposes one trivial tool, `ping`, so it looks like any
//! other server. See `docs/MCP.md`, "Trying alerts with the test server".
//!
//! Run it: `cargo run -p uia-mcp --example alert_feed_server`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, RoleServer, SubscriptionContext};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::broadcast;

const FEED_URI: &str = "uia-test://alerts";
const FEED_MIME: &str = "application/vnd.uia.alerts+json";
const DEFAULT_INTERVAL_SECS: u64 = 15;
const MEETING_LINK: &str = "https://meet.google.com/abc-defg-hij";

/// The shared feed: the entries so far, and a ping per append for every open
/// `subscriptions/listen` stream.
#[derive(Clone)]
struct Feed {
    entries: Arc<Mutex<Vec<Value>>>,
    updated: broadcast::Sender<()>,
}

impl Feed {
    fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
            updated: broadcast::channel(16).0,
        }
    }

    fn append(&self, entry: Value) {
        self.entries.lock().expect("feed lock").push(entry);
        // No listener open is fine: the entry is still in the feed.
        let _ = self.updated.send(());
    }

    fn body(&self) -> String {
        json!({ "alerts": *self.entries.lock().expect("feed lock") }).to_string()
    }
}

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    chrono::DateTime::from_timestamp(i64::try_from(secs).unwrap_or(0), 0)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn meeting_stage(stage: &str, summary: &str) -> Value {
    json!({
        "kind": "meeting",
        "id": "demo-1",
        "stage": stage,
        "title": "Weekly sync",
        "summary": summary,
        "created_at": now(),
        "action": { "label": "Join", "url": MEETING_LINK },
    })
}

/// The entry to append at step `index`, or `None` once the sequence is over.
fn entry_at(index: usize) -> Option<Value> {
    Some(match index {
        0 => meeting_stage("T-10", "Weekly sync starts in 10 minutes"),
        1 => meeting_stage("T-5", "Weekly sync starts in 5 minutes"),
        2 => meeting_stage("T-2", "Weekly sync starts in 2 minutes"),
        3 => meeting_stage("T-0", "Weekly sync starts now"),
        4 => json!({
            "kind": "meeting",
            "id": "demo-2",
            "stage": "invited",
            "summary": "Sam invited you to Design review",
            "created_at": now(),
        }),
        5 => json!({
            "kind": "note",
            "id": "demo-3",
            "summary": "A plain alert with no action button",
            "created_at": now(),
        }),
        // No `summary`: UIA must skip it.
        6 => json!({ "kind": "note", "id": "demo-4", "title": "Entry without a summary" }),
        _ => return None,
    })
}

fn interval() -> Duration {
    let secs = std::env::var("ALERT_FEED_INTERVAL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

struct AlertFeedServer {
    feed: Feed,
}

impl ServerHandler for AlertFeedServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = json!({ "type": "object", "properties": {} });
        let schema = schema.as_object().cloned().expect("an object literal");
        Ok(ListToolsResult {
            tools: vec![Tool::new(
                "ping",
                "Answers pong. Only here so the test server looks like a normal one.",
                Arc::new(schema),
            )],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
        if request.name != "ping" {
            return Err(ErrorData::invalid_params("unknown tool", None));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text("pong")]).into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![Resource::new(FEED_URI, "alerts").with_mime_type(FEED_MIME)],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        if request.uri != FEED_URI {
            return Err(ErrorData::resource_not_found("unknown resource", None));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(self.feed.body(), request.uri).with_mime_type(FEED_MIME),
        ])
        .into())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let mut updated = self.feed.updated.subscribe();
        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                ping = updated.recv() => match ping {
                    // Lagged: several appends coalesce into one "read it again".
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if context.sink().notify_resource_updated(FEED_URI).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let feed = Feed::new();
    let every = interval();
    let producer = feed.clone();
    tokio::spawn(async move {
        for index in 0.. {
            tokio::time::sleep(every).await;
            let Some(entry) = entry_at(index) else { break };
            producer.append(entry);
        }
    });
    let server = AlertFeedServer { feed }.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

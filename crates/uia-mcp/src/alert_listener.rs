// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Watches a connected MCP server's alert feeds over `subscriptions/listen`
//! (MCP 2026-07-28) and forwards new alerts to a sink.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use rmcp::model::{
    ReadResourceRequestParams, ResourceContents, ServerNotification, SubscriptionFilter,
};
use rmcp::service::SubscriptionEnd;
use rmcp::{Peer, RoleClient};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::alert::{Alert, AlertGate, is_alert_feed, parse_feed};

/// The slice of a server's alert settings the listener reads. Shared behind a
/// lock so a toggle applies without a reconnect.
#[derive(Clone, Debug)]
pub struct AlertSettingsView {
    /// False silences the server: its alerts are dropped.
    pub show: bool,
    /// A feed URI to use instead of discovering one by MIME type.
    pub alert_resource: Option<String>,
}

impl Default for AlertSettingsView {
    fn default() -> Self {
        Self {
            show: true,
            alert_resource: None,
        }
    }
}

pub type SharedAlertSettings = Arc<RwLock<AlertSettingsView>>;

/// Reopen delay after a stream ends without the server saying goodbye.
const REOPEN_DELAY: Duration = Duration::from_secs(5);

/// How one listen session ended.
enum SessionEnd {
    /// Nothing more to do: no feed, rejected, or the sink is gone.
    Stop,
    /// The server's resource list changed; discover again.
    Rediscover,
    /// The stream ended without a final result. `was_open` is true when it
    /// had been acknowledged first (the connection was healthy), which earns
    /// a fresh retry.
    Abrupt { was_open: bool },
}

/// One reopen per drop: a stream that had been acknowledged before it ended
/// earns a fresh retry; a reopen that fails before that does not.
#[derive(Default)]
struct ReopenBudget {
    spent: bool,
}

impl ReopenBudget {
    fn may_reopen_after_abrupt_end(&mut self, was_open: bool) -> bool {
        if was_open {
            self.spent = false;
        }
        !std::mem::replace(&mut self.spent, true)
    }
}

/// Watch `server`'s alert feeds until it rejects
/// `subscriptions/listen`, the connection drops or `sink` closes.
///
/// Discovers feeds (the `alert_resource` override, else every resource with
/// the alert-feed MIME type), listens for updates to them and for list
/// changes, and sends each new alert to `sink` unless `settings.show` is off.
/// On a list change it rediscovers and reopens; on an abrupt end it reopens
/// once after 5 s.
pub fn spawn_alert_listener(
    server: String,
    peer: Peer<RoleClient>,
    settings: SharedAlertSettings,
    sink: UnboundedSender<Alert>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut listener = Listener {
            server,
            peer,
            settings,
            sink,
            gate: AlertGate::new(),
            known_feeds: HashSet::new(),
        };
        let mut retry = ReopenBudget::default();
        loop {
            match listener.session().await {
                SessionEnd::Stop => return,
                SessionEnd::Rediscover => {}
                SessionEnd::Abrupt { was_open } => {
                    if !retry.may_reopen_after_abrupt_end(was_open) {
                        return;
                    }
                    tokio::time::sleep(REOPEN_DELAY).await;
                }
            }
        }
    })
}

struct Listener {
    server: String,
    peer: Peer<RoleClient>,
    settings: SharedAlertSettings,
    sink: UnboundedSender<Alert>,
    gate: AlertGate,
    /// Feeds already read once on this connection; a feed seen for the first
    /// time later gets its own baseline.
    known_feeds: HashSet<String>,
}

impl Listener {
    fn show(&self) -> bool {
        self.settings.read().map(|s| s.show).unwrap_or(true)
    }

    fn override_uri(&self) -> Option<String> {
        self.settings
            .read()
            .ok()
            .and_then(|s| s.alert_resource.clone())
            .filter(|uri| !uri.trim().is_empty())
    }

    /// The feed URIs to watch, or `None` when the server cannot list resources.
    async fn discover(&self) -> Option<Vec<String>> {
        if let Some(uri) = self.override_uri() {
            return Some(vec![uri]);
        }
        match self.peer.list_all_resources().await {
            Ok(resources) => Some(
                resources
                    .into_iter()
                    .filter(|r| is_alert_feed(r.mime_type.as_deref()))
                    .map(|r| r.uri.clone())
                    .collect(),
            ),
            Err(e) => {
                tracing::info!(server = %self.server, error = %e, "cannot list resources; no alerts from this server");
                None
            }
        }
    }

    async fn session(&mut self) -> SessionEnd {
        let Some(feeds) = self.discover().await else {
            return SessionEnd::Stop;
        };
        tracing::debug!(server = %self.server, feeds = feeds.len(), "alert feeds discovered");
        let mut filter = SubscriptionFilter::builder().resources_list_changed();
        for uri in &feeds {
            filter = filter.resource_subscription(uri.as_str());
        }
        let mut subscription = match self.peer.listen(filter.build()).await {
            Ok(subscription) => subscription,
            Err(e) => {
                tracing::info!(server = %self.server, error = %e, "server does not support subscriptions/listen; no alerts from this server");
                return SessionEnd::Stop;
            }
        };
        let accepted = subscription.acknowledged();
        let watched: Vec<String> = accepted
            .resource_subscriptions
            .iter()
            .flatten()
            .filter(|uri| feeds.contains(uri))
            .cloned()
            .collect();
        if watched.is_empty() && accepted.resources_list_changed != Some(true) {
            tracing::info!(server = %self.server, "server accepted no alert subscription; no alerts from this server");
            return SessionEnd::Stop;
        }
        // Read every feed once now: it is the baseline (or, after a reopen,
        // catches what arrived while the stream was down).
        for uri in &watched {
            if self.known_feeds.insert(uri.clone()) && self.known_feeds.len() > 1 {
                self.gate.reset_baseline(&self.server);
            }
            if !self.read_and_forward(uri).await {
                return SessionEnd::Stop;
            }
        }
        loop {
            match subscription.next().await {
                Ok(Some(ServerNotification::ResourceUpdatedNotification(n))) => {
                    if watched.contains(&n.params.uri)
                        && !self.read_and_forward(&n.params.uri).await
                    {
                        return SessionEnd::Stop;
                    }
                }
                Ok(Some(ServerNotification::ResourceListChangedNotification(_))) => {
                    return SessionEnd::Rediscover;
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    return match subscription.end() {
                        Some(SubscriptionEnd::Abrupt | SubscriptionEnd::Lagged { .. }) => {
                            SessionEnd::Abrupt { was_open: true }
                        }
                        _ => SessionEnd::Stop,
                    };
                }
                Err(e) => {
                    tracing::debug!(server = %self.server, error = %e, "alert subscription failed");
                    return SessionEnd::Abrupt { was_open: true };
                }
            }
        }
    }

    /// Read one feed and forward its new alerts. False when the sink is closed.
    async fn read_and_forward(&mut self, uri: &str) -> bool {
        let text = match self
            .peer
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
        {
            Ok(result) => result.contents.into_iter().find_map(|c| match c {
                ResourceContents::TextResourceContents { text, .. } => Some(text),
                _ => None,
            }),
            Err(e) => {
                tracing::debug!(server = %self.server, error = %e, "could not read the alert feed");
                return true;
            }
        };
        let alerts = text
            .map(|t| parse_feed(&self.server, &t))
            .unwrap_or_default();
        let new = self.gate.admit(alerts, SystemTime::now());
        self.gate.mark_read(&self.server);
        if !self.show() {
            return true;
        }
        for alert in new {
            if self.sink.send(alert).is_err() {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::client::McpExecutor;
    use rmcp::ServiceExt;
    use rmcp::model::{
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
        ServerConfig, SubscriptionFilter,
    };
    use rmcp::service::{RequestContext, RoleServer, SubscriptionContext};
    use rmcp::{ErrorData, ServerHandler};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};
    use tokio::sync::{Notify, broadcast, mpsc};

    const FEED: &str = "mymy://alerts";
    const MIME: &str = crate::alert::ALERT_FEED_MIME;

    #[derive(Clone, Debug)]
    enum Event {
        Updated(String),
        ListChanged,
    }

    /// What the in-process server serves, and what the test pokes.
    struct World {
        resources: Mutex<Vec<(String, Option<&'static str>)>>,
        contents: Mutex<HashMap<String, String>>,
        events: broadcast::Sender<Event>,
        listens: AtomicUsize,
        listen_opened: Notify,
        list_calls: AtomicUsize,
    }

    impl World {
        fn new(resources: Vec<(&str, Option<&'static str>)>) -> Arc<Self> {
            Arc::new(Self {
                resources: Mutex::new(
                    resources
                        .into_iter()
                        .map(|(u, m)| (u.to_string(), m))
                        .collect(),
                ),
                contents: Mutex::new(HashMap::new()),
                events: broadcast::channel(16).0,
                listens: AtomicUsize::new(0),
                listen_opened: Notify::new(),
                list_calls: AtomicUsize::new(0),
            })
        }

        fn set_feed(&self, uri: &str, summaries: &[(&str, &str)]) {
            let alerts: Vec<_> = summaries
                .iter()
                .map(|(id, summary)| {
                    serde_json::json!({
                        "id": id, "summary": summary,
                        "created_at": chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
                            .to_rfc3339(),
                    })
                })
                .collect();
            self.contents.lock().unwrap().insert(
                uri.to_string(),
                serde_json::json!({ "alerts": alerts }).to_string(),
            );
        }

        fn publish(&self, event: Event) {
            let _ = self.events.send(event);
        }

        /// Wait until the server has accepted `count` listens.
        async fn listening(&self, count: usize) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while self.listens.load(Ordering::SeqCst) < count {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the server never saw listen #{count}"
                );
                tokio::time::timeout(Duration::from_millis(50), self.listen_opened.notified())
                    .await
                    .ok();
            }
        }
    }

    struct FeedServer {
        world: Arc<World>,
    }

    impl ServerHandler for FeedServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(
                ServerCapabilities::builder()
                    .enable_resources()
                    .enable_resources_subscribe()
                    .enable_resources_list_changed()
                    .build(),
            )
        }

        async fn list_resources(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListResourcesResult, ErrorData> {
            self.world.list_calls.fetch_add(1, Ordering::SeqCst);
            let resources = self
                .world
                .resources
                .lock()
                .unwrap()
                .iter()
                .map(|(uri, mime)| {
                    let resource = Resource::new(uri.as_str(), "r");
                    match mime {
                        Some(mime) => resource.with_mime_type(*mime),
                        None => resource,
                    }
                })
                .collect();
            Ok(ListResourcesResult {
                resources,
                ..Default::default()
            })
        }

        async fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<ReadResourceResponse, ErrorData> {
            let text = self
                .world
                .contents
                .lock()
                .unwrap()
                .get(&request.uri)
                .cloned()
                .unwrap_or_else(|| r#"{"alerts":[]}"#.to_string());
            Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, request.uri).with_mime_type(MIME),
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
            let mut events = self.world.events.subscribe();
            self.world.listens.fetch_add(1, Ordering::SeqCst);
            self.world.listen_opened.notify_waiters();
            loop {
                tokio::select! {
                    _ = context.cancelled() => return Ok(()),
                    event = events.recv() => match event {
                        Ok(Event::Updated(uri)) => {
                            let _ = context.sink().notify_resource_updated(uri).await;
                        }
                        Ok(Event::ListChanged) => {
                            let _ = context.sink().notify_resource_list_changed().await;
                        }
                        Err(_) => return Ok(()),
                    },
                }
            }
        }
    }

    struct Harness {
        world: Arc<World>,
        executor: McpExecutor,
        settings: SharedAlertSettings,
        alerts: mpsc::UnboundedReceiver<Alert>,
        listener: JoinHandle<()>,
    }

    /// A server that predates 2026-07-28 the way real ones do: it answers
    /// `server/discover` and `subscriptions/listen` with "method not found",
    /// negotiates 2025-11-25 at `initialize`, and offers one tool and one
    /// alert-feed resource.
    pub(crate) fn spawn_legacy_server(io: tokio::io::DuplexStream) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(io);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let (Some(id), Some(method)) = (request.get("id"), request["method"].as_str())
                else {
                    continue; // a notification or a response: nothing to answer
                };
                let result = match method {
                    "initialize" => Ok(serde_json::json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {}, "resources": {"subscribe": true}},
                        "serverInfo": {"name": "legacy", "version": "1"},
                    })),
                    "tools/list" => Ok(serde_json::json!({"tools": [
                        {"name": "echo", "description": "d", "inputSchema": {"type": "object"}}
                    ]})),
                    "resources/list" => Ok(serde_json::json!({"resources": [
                        {"uri": FEED, "name": "alerts", "mimeType": MIME}
                    ]})),
                    _ => Err(serde_json::json!({"code": -32601, "message": "Method not found"})),
                };
                let reply = match result {
                    Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => serde_json::json!({"jsonrpc": "2.0", "id": id, "error": error}),
                };
                if write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    async fn start(world: Arc<World>, legacy_only: bool, settings: AlertSettingsView) -> Harness {
        let (server_io, client_io) = tokio::io::duplex(16 * 1024);
        if legacy_only {
            spawn_legacy_server(server_io);
        } else {
            let server = FeedServer {
                world: world.clone(),
            };
            tokio::spawn(async move {
                let running = server.serve(server_io).await.expect("server starts");
                let _ = running.waiting().await;
            });
        }
        let executor = McpExecutor::connect_transport("mymy", client_io)
            .await
            .expect("client connects");
        let settings = Arc::new(RwLock::new(settings));
        let (sink, alerts) = mpsc::unbounded_channel();
        let listener = executor.start_alert_listener(settings.clone(), sink);
        Harness {
            world,
            executor,
            settings,
            alerts,
            listener,
        }
    }

    async fn next_alert(alerts: &mut mpsc::UnboundedReceiver<Alert>) -> Alert {
        tokio::time::timeout(Duration::from_secs(3), alerts.recv())
            .await
            .expect("an alert arrives")
            .expect("the sink is open")
    }

    async fn no_alert_within(alerts: &mut mpsc::UnboundedReceiver<Alert>, wait: Duration) {
        if let Ok(Some(alert)) = tokio::time::timeout(wait, alerts.recv()).await {
            panic!("unexpected alert: {alert:?}");
        }
    }

    #[test]
    fn two_separate_abrupt_ends_of_a_healthy_stream_are_both_recovered() {
        let mut budget = ReopenBudget::default();
        assert!(budget.may_reopen_after_abrupt_end(true));
        // The reopened stream came up, then was cut again days later.
        assert!(budget.may_reopen_after_abrupt_end(true));
        // A reopen that never came up gets no second retry.
        assert!(budget.may_reopen_after_abrupt_end(true));
        assert!(!budget.may_reopen_after_abrupt_end(false));
    }

    #[tokio::test]
    async fn a_new_feed_entry_reaches_the_sink_once() {
        let world = World::new(vec![(FEED, Some(MIME))]);
        let mut h = start(world.clone(), false, AlertSettingsView::default()).await;
        h.world.listening(1).await;
        world.set_feed(FEED, &[("e1", "Weekly sync starts in 5 minutes")]);
        world.publish(Event::Updated(FEED.into()));
        let alert = next_alert(&mut h.alerts).await;
        assert_eq!(alert.server, "mymy");
        assert_eq!(alert.summary, "Weekly sync starts in 5 minutes");
        // The same entry on a re-read is not delivered again.
        world.publish(Event::Updated(FEED.into()));
        no_alert_within(&mut h.alerts, Duration::from_millis(300)).await;
    }

    #[tokio::test]
    async fn the_override_uri_is_used_without_discovery() {
        // No MIME type, so only the override can find it.
        let world = World::new(vec![(FEED, None)]);
        let settings = AlertSettingsView {
            alert_resource: Some(FEED.into()),
            ..Default::default()
        };
        let mut h = start(world.clone(), false, settings).await;
        h.world.listening(1).await;
        world.set_feed(FEED, &[("e1", "override alert")]);
        world.publish(Event::Updated(FEED.into()));
        assert_eq!(next_alert(&mut h.alerts).await.summary, "override alert");
        assert_eq!(world.list_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_server_without_a_feed_sends_nothing() {
        let world = World::new(vec![("file:///notes.txt", Some("text/plain"))]);
        let mut h = start(world.clone(), false, AlertSettingsView::default()).await;
        h.world.listening(1).await;
        world.set_feed("file:///notes.txt", &[("n", "not an alert")]);
        world.publish(Event::Updated("file:///notes.txt".into()));
        no_alert_within(&mut h.alerts, Duration::from_millis(300)).await;
    }

    #[tokio::test]
    async fn a_server_that_rejects_listen_ends_the_listener_without_error() {
        // A server that only speaks 2025-11-25 has no `subscriptions/listen`.
        let world = World::new(vec![(FEED, Some(MIME))]);
        let mut h = start(world.clone(), true, AlertSettingsView::default()).await;
        tokio::time::timeout(Duration::from_secs(3), &mut h.listener)
            .await
            .expect("the listener ends")
            .expect("it ended without panicking");
        assert_eq!(world.listens.load(Ordering::SeqCst), 0);
        assert!(h.alerts.try_recv().is_err());
    }

    #[tokio::test]
    async fn nothing_reaches_the_sink_when_show_is_false() {
        let world = World::new(vec![(FEED, Some(MIME))]);
        let settings = AlertSettingsView {
            show: false,
            ..Default::default()
        };
        let mut h = start(world.clone(), false, settings).await;
        h.world.listening(1).await;
        world.set_feed(FEED, &[("e1", "silenced")]);
        world.publish(Event::Updated(FEED.into()));
        no_alert_within(&mut h.alerts, Duration::from_millis(300)).await;
        // Turning it on later shows only what is new from then on.
        h.settings.write().unwrap().show = true;
        world.set_feed(FEED, &[("e1", "silenced"), ("e2", "audible")]);
        world.publish(Event::Updated(FEED.into()));
        assert_eq!(next_alert(&mut h.alerts).await.summary, "audible");
    }

    #[tokio::test]
    async fn a_list_change_that_adds_a_feed_starts_listening_to_it() {
        let world = World::new(vec![]);
        let mut h = start(world.clone(), false, AlertSettingsView::default()).await;
        h.world.listening(1).await;
        world
            .resources
            .lock()
            .unwrap()
            .push((FEED.to_string(), Some(MIME)));
        world.publish(Event::ListChanged);
        // The listener reopens its stream with the new feed.
        h.world.listening(2).await;
        world.set_feed(FEED, &[("e1", "late feed")]);
        world.publish(Event::Updated(FEED.into()));
        assert_eq!(next_alert(&mut h.alerts).await.summary, "late feed");
        let _ = &h.executor;
    }
}

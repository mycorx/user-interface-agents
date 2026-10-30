// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Spike: proves the pinned `rmcp` negotiates MCP 2026-07-28 and delivers
//! `notifications/resources/updated` over `subscriptions/listen`, against an
//! in-process rmcp server on a duplex transport. The harness here (a server
//! with one alert-feed resource that publishes on `listen`) is the template
//! for the alert listener's tests.

use std::time::Duration;

use rmcp::model::{
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerConfig, ServerNotification, SubscriptionFilter,
};
use rmcp::service::{RequestContext, RoleServer, SubscriptionContext};
use rmcp::{ClientLifecycleMode, ClientServiceExt, ServerHandler, ServiceExt};

const FEED_URI: &str = "mymy://alerts/feed";
const FEED_MIME: &str = "application/vnd.uia.alerts+json";

struct FeedServer;

impl ServerHandler for FeedServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_subscribe()
                .build(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListResourcesResult {
            resources: vec![
                rmcp::model::Resource::new(FEED_URI, "alerts").with_mime_type(FEED_MIME),
            ],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, rmcp::ErrorData> {
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text("[]", request.uri).with_mime_type(FEED_MIME),
        ])
        .into())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), rmcp::ErrorData> {
        context
            .sink()
            .notify_resource_updated(FEED_URI)
            .await
            .expect("the feed uri was requested, so it is accepted");
        context.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn a_client_on_2026_07_28_receives_resource_updated_over_listen()
-> Result<(), Box<dyn std::error::Error>> {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    tokio::spawn(async move {
        let server = FeedServer
            .serve(server_transport)
            .await
            .expect("server starts");
        let _ = server.waiting().await;
    });

    // `().serve` is the legacy `initialize` handshake and tops out at 2025-11-25;
    // `Auto` probes `server/discover` and falls back to legacy for old servers.
    let client = ()
        .serve_with_lifecycle(
            client_transport,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: None,
            },
        )
        .await?;

    let negotiated = client
        .peer_info()
        .expect("handshake completed")
        .protocol_version
        .clone();
    assert_eq!(negotiated, ProtocolVersion::V_2026_07_28);

    let mut subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(FEED_URI)
                .build(),
        )
        .await?;
    let notification = tokio::time::timeout(Duration::from_secs(2), subscription.next())
        .await??
        .expect("the subscription is still open");
    let ServerNotification::ResourceUpdatedNotification(updated) = notification else {
        panic!("expected resources/updated");
    };
    assert_eq!(updated.params.uri, FEED_URI);
    Ok(())
}

// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

pub mod fake;
pub use fake::FakeExecutor;

use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDescriptor {
    /// Namespaced `server.tool` so two MCP servers cannot collide.
    pub name: String,
    pub description: String,
    /// JSON Schema, as supplied by MCP `inputSchema`.
    pub input_schema: serde_json::Value,
    /// Reserved for SP2's confirmation UI. Always false in SP1.
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),
    #[error("tool timed out after {0:?}")]
    Timeout(Duration),
    #[error("tool transport error: {0}")]
    Transport(String),
    /// The server answered 401/403 with a `WWW-Authenticate` challenge and no
    /// usable bearer token was available. Distinct from `Transport` on
    /// purpose: a caller that wants to *start* an OAuth flow (rather than
    /// report "unreachable") needs the raw challenge header, which a
    /// stringified `Transport` error would have already thrown away. Not a
    /// secret — the challenge only names how to authenticate, never a
    /// credential — so it is safe to carry and to print.
    #[error("authorization required: {0}")]
    AuthRequired(String),
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError>;
    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        deadline: Duration,
    ) -> Result<ToolResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn fake_executor_returns_its_canned_result() {
        let exec = FakeExecutor::new().with_result("get_time", ToolResult::ok("12:00"));
        let r = exec
            .execute("get_time", serde_json::json!({}), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(r.content, "12:00");
        assert!(!r.is_error);
    }

    #[tokio::test]
    async fn fake_executor_records_calls_for_assertion() {
        let exec = FakeExecutor::new().with_result("t", ToolResult::ok("x"));
        exec.execute("t", serde_json::json!({"a": 1}), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(exec.calls().len(), 1);
        assert_eq!(exec.calls()[0].0, "t");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error_not_a_panic() {
        let exec = FakeExecutor::new();
        let err = exec
            .execute("nope", serde_json::json!({}), Duration::from_secs(1))
            .await;
        assert!(matches!(err, Err(ToolError::NotFound(_))));
    }

    #[test]
    fn tool_result_error_constructor_sets_the_flag() {
        let r = ToolResult::error("boom");
        assert!(r.is_error);
        assert_eq!(r.content, "boom");
    }
}

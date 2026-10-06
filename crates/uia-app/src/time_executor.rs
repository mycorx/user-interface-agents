// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! `get_current_time`, layered over whatever executor it wraps.
//!
//! The second built-in tool, after `switch_persona`, and it takes the same
//! shape: a decorator, rather than an entry in MCP dispatch. `uia-mcp` never
//! learns that built-ins exist.

use crate::time::{
    Clock, GET_CURRENT_TIME_TOOL, LocalZone, get_current_time_descriptor, render_time, resolve_zone,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use uia_core::tools::{ToolDescriptor, ToolError, ToolExecutor, ToolResult};

pub struct TimeExecutor {
    inner: Arc<dyn ToolExecutor>,
    clock: Arc<dyn Clock>,
    zone: Arc<dyn LocalZone>,
}

impl TimeExecutor {
    pub fn new(
        inner: Arc<dyn ToolExecutor>,
        clock: Arc<dyn Clock>,
        zone: Arc<dyn LocalZone>,
    ) -> Self {
        Self { inner, clock, zone }
    }
}

#[async_trait]
impl ToolExecutor for TimeExecutor {
    /// Unlike `PersonaExecutor`, a wrapped failure does not propagate. The
    /// clock is the one tool that must not depend on an MCP server being
    /// reachable — that independence is half the reason it is built in
    /// rather than shipped as a bundle.
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
        let mut tools = match self.inner.list_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                eprintln!("tools: wrapped executor could not list its tools: {e}");
                Vec::new()
            }
        };
        // Prepended, so with `persona_tools` outermost the model reads
        // switch_persona, then this, then whatever servers are installed.
        tools.insert(0, get_current_time_descriptor());
        Ok(tools)
    }

    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<ToolResult, ToolError> {
        if name != GET_CURRENT_TIME_TOOL {
            return self.inner.execute(name, args, timeout).await;
        }

        // A wrong argument is a normal result, not a `ToolError`: the tool ran
        // fine and the model should read the reason and retry, rather than the
        // user hearing the session report a failure.
        let requested = match args.get("timezone") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            Some(other) => {
                return Ok(ToolResult::error(format!(
                    "the timezone argument must be a string, got {other}"
                )));
            }
        };

        match resolve_zone(requested, self.zone.local()) {
            Ok(tz) => Ok(ToolResult::ok(render_time(self.clock.now_utc(), tz))),
            Err(message) => Ok(ToolResult::error(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{FixedClock, FixedZone};
    use chrono::{DateTime, Utc};
    use chrono_tz::Tz;
    use uia_core::tools::FakeExecutor;

    fn executor_over(inner: Arc<dyn ToolExecutor>) -> TimeExecutor {
        let when: DateTime<Utc> = "2026-09-07T13:10:04Z".parse().unwrap();
        let zone: Tz = "Australia/Melbourne".parse().unwrap();
        TimeExecutor::new(inner, Arc::new(FixedClock(when)), Arc::new(FixedZone(zone)))
    }

    fn with_one_mcp_tool() -> Arc<dyn ToolExecutor> {
        Arc::new(FakeExecutor::new().with_result("files.read", ToolResult::ok("inner")))
    }

    #[tokio::test]
    async fn the_clock_is_declared_alongside_the_wrapped_tools() {
        let tools = executor_over(with_one_mcp_tool())
            .list_tools()
            .await
            .unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"get_current_time"), "{names:?}");
        assert!(names.contains(&"files.read"), "{names:?}");
    }

    #[tokio::test]
    async fn the_clock_is_declared_first() {
        // With persona_tools outermost this yields switch_persona,
        // get_current_time, then the installed servers' tools.
        let tools = executor_over(with_one_mcp_tool())
            .list_tools()
            .await
            .unwrap();
        assert_eq!(tools[0].name, "get_current_time");
    }

    #[tokio::test]
    async fn a_wrapped_executor_that_cannot_list_does_not_take_the_clock_with_it() {
        struct Broken;
        #[async_trait]
        impl ToolExecutor for Broken {
            async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
                Err(ToolError::Transport("nope".into()))
            }
            async fn execute(
                &self,
                _: &str,
                _: serde_json::Value,
                _: Duration,
            ) -> Result<ToolResult, ToolError> {
                unreachable!()
            }
        }
        let tools = executor_over(Arc::new(Broken)).list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_current_time");
    }

    #[tokio::test]
    async fn calling_the_clock_renders_the_injected_instant() {
        let out = executor_over(with_one_mcp_tool())
            .execute(
                GET_CURRENT_TIME_TOOL,
                serde_json::json!({}),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(
            out.content.contains("Australia/Melbourne"),
            "{}",
            out.content
        );
        assert!(out.content.contains("11:10 pm"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_explicit_timezone_wins_over_the_device_zone() {
        let out = executor_over(with_one_mcp_tool())
            .execute(
                GET_CURRENT_TIME_TOOL,
                serde_json::json!({"timezone": "Europe/London"}),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out.content.contains("Europe/London"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_unknown_zone_is_an_error_result_not_a_transport_error() {
        let out = executor_over(with_one_mcp_tool())
            .execute(
                GET_CURRENT_TIME_TOOL,
                serde_json::json!({"timezone": "PST"}),
                Duration::from_secs(5),
            )
            .await
            .expect("a bad argument must not fail the transport");
        assert!(out.is_error);
        assert!(out.content.contains("PST"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_non_string_timezone_is_an_error_result_not_a_panic() {
        let out = executor_over(with_one_mcp_tool())
            .execute(
                GET_CURRENT_TIME_TOOL,
                serde_json::json!({"timezone": 42}),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn every_other_tool_passes_through_untouched() {
        let inner = FakeExecutor::new().with_result("files.read", ToolResult::ok("inner"));
        let spy = inner.clone();
        let out = executor_over(Arc::new(inner))
            .execute(
                "files.read",
                serde_json::json!({"path": "a.txt"}),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(out.content, "inner");
        assert_eq!(
            spy.calls(),
            vec![("files.read".into(), serde_json::json!({"path": "a.txt"}))]
        );
    }
}

// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default, Clone)]
pub struct FakeExecutor {
    results: HashMap<String, ToolResult>,
    /// When set, `execute` sleeps this long — used to drive timeout tests.
    delay: Option<Duration>,
    calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_result(mut self, name: &str, r: ToolResult) -> Self {
        self.results.insert(name.to_string(), r);
        self
    }
    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = Some(d);
        self
    }
    pub fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolExecutor for FakeExecutor {
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
        Ok(self
            .results
            .keys()
            .map(|n| ToolDescriptor {
                name: n.clone(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
                requires_confirmation: false,
            })
            .collect())
    }

    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        deadline: Duration,
    ) -> Result<ToolResult, ToolError> {
        self.calls.lock().unwrap().push((name.to_string(), args));
        if let Some(d) = self.delay {
            if tokio::time::timeout(deadline, tokio::time::sleep(d))
                .await
                .is_err()
            {
                return Err(ToolError::Timeout(deadline));
            }
        }
        self.results
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::NotFound(name.to_string()))
    }
}

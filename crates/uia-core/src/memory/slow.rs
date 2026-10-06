// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use super::*;
use std::time::Duration;

/// Test adapter that can stall, so the recall deadline is directly assertable.
pub struct SlowMemory {
    delay: Option<Duration>,
    items: Vec<MemoryItem>,
}

impl SlowMemory {
    pub fn instant(items: Vec<MemoryItem>) -> Self {
        Self { delay: None, items }
    }
    pub fn delayed(delay: Duration, items: Vec<MemoryItem>) -> Self {
        Self {
            delay: Some(delay),
            items,
        }
    }
}

#[async_trait]
impl ConversationMemory for SlowMemory {
    async fn recall(
        &self,
        _ctx: &MemoryContext,
        _q: Option<&str>,
    ) -> Result<Vec<MemoryItem>, MemoryError> {
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        Ok(self.items.clone())
    }
    async fn record(&self, _ctx: &MemoryContext, _t: &Turn) -> Result<(), MemoryError> {
        Ok(())
    }
}

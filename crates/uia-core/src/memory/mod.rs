// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

pub mod slow;
pub use slow::SlowMemory;

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct MemoryContext {
    pub user_id: String,
    pub conversation_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryKind {
    Fact,
    Preference,
    Summary,
    /// Escape hatch: backends have their own taxonomies and this port must not
    /// privilege any one vendor's.
    Other(String),
}

/// The `MemoryKind::Other` tag on a recalled line the user spoke.
///
/// Lives here rather than with the file backend because both sides need it:
/// the adapter tags recalled lines, and every engine reads the tag to decide
/// which role to replay each line under. `recall` flattens an exchange into
/// one item per half, so without this the pairing is gone and an engine would
/// have to attribute lines by guesswork.
pub const RECALL_ROLE_USER: &str = "user";

/// The `MemoryKind::Other` tag on a recalled line the assistant spoke.
pub const RECALL_ROLE_ASSISTANT: &str = "assistant";

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryItem {
    pub content: String,
    pub kind: MemoryKind,
    pub score: Option<f32>,
}

/// One completed exchange. Populated from transcript events, so both fields
/// are `None` when transcription is disabled.
#[derive(Debug, Clone)]
pub struct Turn {
    pub user: Option<String>,
    pub assistant: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum MemoryError {
    #[error("memory backend error: {0}")]
    Backend(String),
    #[error("memory recall timed out")]
    Timeout,
}

/// Deliberately vendor-neutral: the backend may end up being AgentCore,
/// Vertex AI Memory Bank, Azure Foundry, SQLite, or an MCP server.
#[async_trait]
pub trait ConversationMemory: Send + Sync {
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: Option<&str>,
    ) -> Result<Vec<MemoryItem>, MemoryError>;
    async fn record(&self, ctx: &MemoryContext, turn: &Turn) -> Result<(), MemoryError>;
}

/// The only implementation in SP1. No AWS dependency, no network.
pub struct NullMemory;

#[async_trait]
impl ConversationMemory for NullMemory {
    async fn recall(
        &self,
        _ctx: &MemoryContext,
        _q: Option<&str>,
    ) -> Result<Vec<MemoryItem>, MemoryError> {
        Ok(vec![])
    }
    async fn record(&self, _ctx: &MemoryContext, _t: &Turn) -> Result<(), MemoryError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_memory_recalls_nothing_and_never_errors() {
        let m = NullMemory;
        let ctx = MemoryContext {
            user_id: "u".into(),
            conversation_id: "c".into(),
        };
        assert_eq!(m.recall(&ctx, None).await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn null_memory_record_is_a_successful_noop() {
        let m = NullMemory;
        let ctx = MemoryContext {
            user_id: "u".into(),
            conversation_id: "c".into(),
        };
        let turn = Turn {
            user: Some("hi".into()),
            assistant: Some("hello".into()),
        };
        assert!(m.record(&ctx, &turn).await.is_ok());
    }
}

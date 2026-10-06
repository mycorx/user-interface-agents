// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Local conversation memory.
//!
//! Lives here rather than in `uia-core` because it touches the filesystem, and
//! core is deliberately free of I/O so it stays headlessly testable
//! (`scripts/check-core-deps.sh`). Core defines the `ConversationMemory` port;
//! this is the desktop app's adapter for it.
//!
//! Why it exists at all: until now the app wired `NullMemory`, whose `recall`
//! returns nothing and whose `record` is a no-op -- and nothing called `record`
//! in the first place, because the session dropped `UserTranscript` and
//! `ModelTranscript` on an unreachable catch-all arm. Every reconnect therefore
//! resumed with the assistant remembering nothing. That was tolerable while
//! reconnects were rare accidents. It is not tolerable now that the app
//! disconnects deliberately -- on an idle timeout, when Settings opens, and
//! when rotating a session before the provider's ceiling.
//!
//! # Why JSONL, and how its one drawback is handled
//!
//! The store is newline-delimited JSON: a header line, then one object per
//! exchange. This is the same shape Ollama and Anthropic use for transcripts,
//! and it is chosen here for two properties a single JSON array cannot offer.
//!
//! *Appends are O(1).* A turn costs one `write` of one line rather than a
//! re-serialisation of the entire history. Recording happens on every spoken
//! exchange, so this is the hot path.
//!
//! *A torn write costs one line, not the file.* An array must be re-encoded
//! whole, so a crash mid-write leaves a truncated document that parses as
//! nothing -- and since an unreadable store loads as empty history (see
//! `load`), that silently destroys the entire conversation. With JSONL a
//! partial final line is simply skipped and every complete line before it
//! survives.
//!
//! The drawback of an append-only log is trimming: enforcing `RETAIN_LIMIT`
//! means removing from the *front*, which a pure append cannot do. Rewriting
//! on every turn to trim would give back the O(1) append and reintroduce the
//! torn-write window, so instead the log is allowed to overshoot and is
//! **compacted** only once it passes `COMPACT_THRESHOLD` -- amortised, one
//! rewrite per `RETAIN_LIMIT` turns. That rewrite is the one place the whole
//! file is replaced, and it is done atomically (temp file, fsync, rename) so
//! there is no instant at which the real path holds a partial document.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uia_core::memory::{
    ConversationMemory, MemoryContext, MemoryError, MemoryItem, MemoryKind, RECALL_ROLE_ASSISTANT,
    RECALL_ROLE_USER, Turn,
};

// `MemoryKind::Other` rather than a new variant: its own doc calls it the
// escape hatch for taxonomies the port must not privilege, and a verbatim
// transcript line is exactly that -- not a Fact, Preference or Summary, all of
// which imply something was distilled.
//
// It carries the speaker, because an engine replaying this as conversation
// history has to know who said each line. `recall` flattens an exchange into
// one item per half, so without a role on the item the pairing is gone and
// every recalled line would have to be attributed by guesswork.

/// How many past exchanges to carry into a new connection.
///
/// A realtime session's setup prompt is paid for on every connect, and this
/// rides on it, so it trades recall against reconnect cost and latency. Twenty
/// exchanges is far more than the "carry on where we left off" case needs while
/// staying small next to the system prompt.
const RECALL_LIMIT: usize = 20;

/// How many exchanges to keep. Larger than `RECALL_LIMIT` so history survives
/// for a future feature that wants more of it, small enough that the file stays
/// trivial to rewrite whole when compaction does come around.
const RETAIN_LIMIT: usize = 200;

/// How many lines on disk before the log is compacted back down to
/// `RETAIN_LIMIT`.
///
/// This is what buys the O(1) append. Compacting the moment the log passes
/// `RETAIN_LIMIT` would mean rewriting the file on *every* subsequent turn,
/// which is precisely the cost JSONL exists to avoid. Allowing it to overshoot
/// to twice that spreads one rewrite over `RETAIN_LIMIT` appends, while
/// bounding the file at roughly double its logical size.
const COMPACT_THRESHOLD: usize = RETAIN_LIMIT * 2;

/// Identifies the file and its layout. Written as the first line so a future
/// change of shape can be detected rather than guessed at.
const SCHEMA: &str = "uia-conversation";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    schema: String,
    v: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct StoredTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assistant: Option<String>,
}

/// In-memory mirror of the log, plus what is needed to decide when to compact.
struct State {
    turns: Vec<StoredTurn>,
    /// Turn lines currently on disk, excluding the header. Tracked rather than
    /// re-counted so deciding to compact never costs a read.
    disk_lines: usize,
}

/// Conversation memory backed by one JSONL file beside the config.
pub struct FileMemory {
    path: PathBuf,
    state: Mutex<State>,
}

impl FileMemory {
    /// Every retained exchange, oldest first, with its two halves still
    /// paired.
    ///
    /// `recall` cannot serve a reader: it caps at `RECALL_LIMIT` because it
    /// is building a prompt, and it flattens each exchange into separate
    /// `MemoryItem`s, which loses who said what to whom. This returns the
    /// whole retained window (`RETAIN_LIMIT`) with the pairing intact.
    ///
    /// Either half may be `None`. An exchange recorded before the session
    /// asked engines to transcribe the user has an assistant half and nothing
    /// else, and nothing can backfill it.
    pub fn turns(&self) -> Vec<Turn> {
        let Ok(state) = self.state.lock() else {
            // A poisoned lock means a writer panicked mid-record. Reporting no
            // history is honest and harmless here; refusing to render the card
            // over it would not be.
            return Vec::new();
        };
        state
            .turns
            .iter()
            .map(|t| Turn {
                user: t.user.clone(),
                assistant: t.assistant.clone(),
            })
            .collect()
    }

    /// `uia-conversation.jsonl`, a sibling of `uia.toml`, like every other
    /// sidecar this app writes.
    pub fn path_for(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("uia-conversation.jsonl")
    }

    /// Reads what is already there.
    ///
    /// A missing or unreadable file is an empty history, never an error: memory
    /// is an enhancement, and refusing to start because a transcript could not
    /// be parsed would be a worse failure than forgetting.
    ///
    /// Individual malformed lines are skipped rather than abandoning the file.
    /// That is the property that makes an interrupted append survivable -- the
    /// damage is confined to the last line, and everything written before it is
    /// still recalled.
    pub fn load(path: PathBuf) -> Self {
        let turns = std::fs::read_to_string(&path)
            .ok()
            .map(|raw| Self::parse(&raw))
            .unwrap_or_default();
        let disk_lines = turns.len();
        Self {
            path,
            state: Mutex::new(State { turns, disk_lines }),
        }
    }

    fn parse(raw: &str) -> Vec<StoredTurn> {
        let mut lines = raw.lines().filter(|l| !l.trim().is_empty());

        // A header from a future version means the rest of the file is in a
        // shape this build does not understand. Reading it as v1 anyway would
        // silently mangle it, so treat the history as empty.
        match lines.next().map(serde_json::from_str::<Header>) {
            Some(Ok(h)) if h.schema == SCHEMA && h.v == SCHEMA_VERSION => {}
            // Either a header this build cannot read, or no parseable header at
            // all. There is no earlier released format to migrate from.
            _ => return Vec::new(),
        }

        let mut turns: Vec<StoredTurn> = lines
            .filter_map(|l| serde_json::from_str::<StoredTurn>(l).ok())
            .collect();
        let overflow = turns.len().saturating_sub(RETAIN_LIMIT);
        if overflow > 0 {
            turns.drain(0..overflow);
        }
        turns
    }

    fn header_line() -> Result<String, MemoryError> {
        let h = Header {
            schema: SCHEMA.to_string(),
            v: SCHEMA_VERSION,
        };
        serde_json::to_string(&h).map_err(|e| MemoryError::Backend(e.to_string()))
    }

    /// One line, appended. The header is written first if the file is new or
    /// empty, so there is no separate initialisation step to get wrong.
    fn append(&self, turn: &StoredTurn) -> Result<(), MemoryError> {
        let line = serde_json::to_string(turn).map_err(|e| MemoryError::Backend(e.to_string()))?;
        let backend = |e: std::io::Error| MemoryError::Backend(e.to_string());

        let needs_header = match std::fs::metadata(&self.path) {
            Ok(m) => m.len() == 0,
            Err(_) => true,
        };

        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(backend)?;

        let mut buf = String::new();
        if needs_header {
            buf.push_str(&Self::header_line()?);
            buf.push('\n');
        }
        buf.push_str(&line);
        buf.push('\n');

        // A single write of the whole record: it does not make the append
        // atomic, but it keeps the damage from an interrupted one to a partial
        // trailing line, which `parse` skips.
        f.write_all(buf.as_bytes()).map_err(backend)
    }

    /// Where a compaction stages its rewrite.
    ///
    /// Its own function so the two properties that make the rename safe --
    /// that it is a *different* file, and that it is in the *same* directory
    /// so the rename cannot cross a filesystem -- can be asserted directly.
    /// Neither is observable from the outside once the rename has happened.
    fn temp_path(&self) -> PathBuf {
        self.path.with_extension("jsonl.tmp")
    }

    /// Rewrites the log down to `turns`, durably. See [`replace_durably`].
    fn compact(&self, turns: &[StoredTurn]) -> Result<(), MemoryError> {
        let mut buf = Self::header_line()?;
        buf.push('\n');
        for t in turns {
            buf.push_str(
                &serde_json::to_string(t).map_err(|e| MemoryError::Backend(e.to_string()))?,
            );
            buf.push('\n');
        }

        replace_durably(&self.temp_path(), &self.path, buf.as_bytes())
            .map_err(|e| MemoryError::Backend(e.to_string()))
    }
}

/// One step of [`replace_durably`].
///
/// These are not a log of what happened -- they are the instructions, and
/// [`REPLACE_PLAN`] is the sequence `replace_durably` executes. That coupling
/// is deliberate and load-bearing.
///
/// An earlier version of this had the function call each operation directly and
/// report a matching token to an observer, so a test could assert the order.
/// It did not work: the token was a separate statement from the operation, so
/// deleting the `sync_all` while leaving `on_step(Synced)` behind passed the
/// whole suite. The test recorded intent rather than fact.
///
/// Driving the operations *from* the plan removes that gap. Dropping `Sync`
/// from the plan is the only way to stop the sync happening, and the plan is
/// asserted. This matters because none of these steps has any in-process
/// effect: the resulting file is byte-identical whether or not anything was
/// ever flushed, and the failures they prevent need a power cut to provoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplaceStep {
    /// Stage the new contents in the temp file.
    Write,
    /// Put those bytes on the device, not merely in the page cache.
    Sync,
    /// Publish them. Atomic on Windows, macOS and Linux alike.
    Rename,
    /// Put the *directory entry* the rename created on the device too. Unix
    /// only; on Windows the rename is already durable once it returns.
    #[cfg(unix)]
    SyncDir,
}

/// The order the steps run in. Each guards a distinct failure:
///
/// - **`Sync` before `Rename`.** Otherwise the rename can publish a file whose
///   contents are still only in the page cache, so a power loss leaves the
///   destination present but empty or partial -- which, under this module's
///   "unreadable means empty" policy, silently discards the whole conversation.
///   This is precisely what a plain `fs::write` to the destination gets wrong.
/// - **`Rename`, never truncate-and-write.** There is no instant at which the
///   destination holds a partial document.
/// - **`SyncDir` after `Rename`.** On POSIX a rename is a directory
///   modification and is no more durable than the file was: the new name can be
///   lost even though the data survived.
#[cfg(unix)]
const REPLACE_PLAN: &[ReplaceStep] = &[
    ReplaceStep::Write,
    ReplaceStep::Sync,
    ReplaceStep::Rename,
    ReplaceStep::SyncDir,
];
#[cfg(not(unix))]
const REPLACE_PLAN: &[ReplaceStep] = &[ReplaceStep::Write, ReplaceStep::Sync, ReplaceStep::Rename];

/// Replaces `dest` with `bytes`, staged through `tmp`, by executing
/// [`REPLACE_PLAN`] -- which is where the ordering and its rationale live.
///
/// `tmp` must be a sibling of `dest`, or the rename may cross a filesystem and
/// fail; [`FileMemory::temp_path`] is what guarantees that, and is tested.
fn replace_durably(tmp: &Path, dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Any failure past the first step leaves a stale temp file, which would
    // make the next attempt fail for an unrelated reason.
    let cleanup = |e: std::io::Error| {
        std::fs::remove_file(tmp).ok();
        e
    };

    let mut staged: Option<std::fs::File> = None;
    for step in REPLACE_PLAN {
        match step {
            ReplaceStep::Write => {
                let mut f = std::fs::File::create(tmp)?;
                f.write_all(bytes).map_err(cleanup)?;
                staged = Some(f);
            }
            ReplaceStep::Sync => {
                if let Some(f) = staged.as_ref() {
                    f.sync_all().map_err(cleanup)?;
                }
            }
            ReplaceStep::Rename => {
                // Closed first: Windows will not rename a file that is open.
                staged = None;
                std::fs::rename(tmp, dest).map_err(cleanup)?;
            }
            #[cfg(unix)]
            ReplaceStep::SyncDir => {
                // Best effort: a filesystem that refuses to open a directory
                // has already published the rename, and failing here would lose
                // a turn that is safely on disk to guard against a power cut
                // that may never come.
                if let Some(dir) = dest.parent() {
                    if let Ok(d) = std::fs::File::open(dir) {
                        d.sync_all().ok();
                    }
                }
            }
        }
    }
    drop(staged);

    Ok(())
}

#[async_trait::async_trait]
impl ConversationMemory for FileMemory {
    /// The most recent exchanges, oldest first, as one item per side of the
    /// conversation. `query` is ignored: this is a transcript, not a search
    /// index, and pretending to rank against a query would be a lie the caller
    /// might come to rely on.
    async fn recall(
        &self,
        _ctx: &MemoryContext,
        _query: Option<&str>,
    ) -> Result<Vec<MemoryItem>, MemoryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MemoryError::Backend("conversation memory lock poisoned".into()))?;
        let start = state.turns.len().saturating_sub(RECALL_LIMIT);
        let mut out = Vec::new();
        for turn in &state.turns[start..] {
            if let Some(user) = &turn.user {
                out.push(MemoryItem {
                    content: user.clone(),
                    kind: MemoryKind::Other(RECALL_ROLE_USER.into()),
                    score: None,
                });
            }
            if let Some(assistant) = &turn.assistant {
                out.push(MemoryItem {
                    content: assistant.clone(),
                    kind: MemoryKind::Other(RECALL_ROLE_ASSISTANT.into()),
                    score: None,
                });
            }
        }
        Ok(out)
    }

    async fn record(&self, _ctx: &MemoryContext, turn: &Turn) -> Result<(), MemoryError> {
        if turn.user.is_none() && turn.assistant.is_none() {
            return Ok(());
        }
        let stored = StoredTurn {
            user: turn.user.clone(),
            assistant: turn.assistant.clone(),
        };

        // What the mutex guards is the decision, not the I/O: a filesystem
        // stall must not block the session's next turn behind a lock the run
        // loop also wants.
        let plan = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| MemoryError::Backend("conversation memory lock poisoned".into()))?;
            state.turns.push(stored.clone());
            let overflow = state.turns.len().saturating_sub(RETAIN_LIMIT);
            if overflow > 0 {
                state.turns.drain(0..overflow);
            }
            state.disk_lines += 1;
            if state.disk_lines > COMPACT_THRESHOLD {
                state.disk_lines = state.turns.len();
                Some(state.turns.clone())
            } else {
                None
            }
        };

        match plan {
            // Compaction writes the new turn along with everything retained, so
            // it replaces the append rather than following it.
            Some(turns) => self.compact(&turns),
            None => self.append(&stored),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> MemoryContext {
        MemoryContext {
            user_id: "u".into(),
            conversation_id: "c".into(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("uia-mem-{}-{}.jsonl", name, std::process::id()));
        std::fs::remove_file(&p).ok();
        p
    }

    async fn record_user(m: &FileMemory, text: &str) {
        m.record(
            &ctx(),
            &Turn {
                user: Some(text.into()),
                assistant: None,
            },
        )
        .await
        .unwrap();
    }

    /// What `recall` cannot give a reader: whole exchanges, still paired.
    #[tokio::test]
    async fn turns_returns_whole_exchanges_oldest_first() {
        let path = temp("turns-paired");
        let m = FileMemory::load(path.clone());
        m.record(
            &ctx(),
            &Turn {
                user: Some("what is the capital of France".into()),
                assistant: Some("Paris".into()),
            },
        )
        .await
        .unwrap();
        m.record(
            &ctx(),
            &Turn {
                user: Some("and of Spain".into()),
                assistant: Some("Madrid".into()),
            },
        )
        .await
        .unwrap();

        let turns = m.turns();
        std::fs::remove_file(&path).ok();

        assert_eq!(turns.len(), 2, "one entry per exchange, not per half");
        assert_eq!(
            turns[0].user.as_deref(),
            Some("what is the capital of France")
        );
        assert_eq!(turns[0].assistant.as_deref(), Some("Paris"));
        assert_eq!(turns[1].user.as_deref(), Some("and of Spain"));
        assert_eq!(turns[1].assistant.as_deref(), Some("Madrid"));
    }

    /// Every exchange recorded before the session asked engines to transcribe
    /// the user looks like this, and nothing can backfill it. A reader has to
    /// receive the half that exists rather than have the entry dropped.
    #[tokio::test]
    async fn turns_keeps_an_exchange_that_only_has_one_half() {
        let path = temp("turns-one-sided");
        let m = FileMemory::load(path.clone());
        m.record(
            &ctx(),
            &Turn {
                user: None,
                assistant: Some("Paris".into()),
            },
        )
        .await
        .unwrap();

        let turns = m.turns();
        std::fs::remove_file(&path).ok();

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, None);
        assert_eq!(turns[0].assistant.as_deref(), Some("Paris"));
    }

    /// A reader must see what a later session would: `turns` reads the same
    /// mirror `load` rebuilt off disk, not just what this process recorded.
    #[tokio::test]
    async fn turns_survives_a_reload_from_disk() {
        let path = temp("turns-reload");
        let before = FileMemory::load(path.clone());
        before
            .record(
                &ctx(),
                &Turn {
                    user: Some("what is the capital of France".into()),
                    assistant: Some("Paris".into()),
                },
            )
            .await
            .unwrap();

        let after = FileMemory::load(path.clone());
        let turns = after.turns();
        std::fs::remove_file(&path).ok();

        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].user.as_deref(),
            Some("what is the capital of France")
        );
        assert_eq!(turns[0].assistant.as_deref(), Some("Paris"));
    }

    /// The whole point: what was said survives a disconnect. Two `FileMemory`
    /// instances over one path stand in for before and after.
    #[tokio::test]
    async fn a_recorded_turn_is_recalled_by_a_later_session() {
        let path = temp("roundtrip");

        let before = FileMemory::load(path.clone());
        before
            .record(
                &ctx(),
                &Turn {
                    user: Some("what is the capital of France".into()),
                    assistant: Some("Paris".into()),
                },
            )
            .await
            .unwrap();

        let after = FileMemory::load(path.clone());
        let items = after.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "what is the capital of France");
        assert_eq!(items[1].content, "Paris");
    }

    /// A half-turn is still worth keeping: an engine with only user
    /// transcription enabled produces exactly this, and discarding it would
    /// lose the question along with the missing answer.
    #[tokio::test]
    async fn a_turn_with_only_one_side_is_still_recorded() {
        let path = temp("halfturn");

        let m = FileMemory::load(path.clone());
        record_user(&m, "just me").await;
        let items = m.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "just me");
    }

    /// An empty turn must not add an entry -- the session flushes on every
    /// `SpeechEnded`, including ones that carried no transcript at all.
    #[tokio::test]
    async fn an_empty_turn_is_not_recorded() {
        let path = temp("empty");

        let m = FileMemory::load(path.clone());
        m.record(
            &ctx(),
            &Turn {
                user: None,
                assistant: None,
            },
        )
        .await
        .unwrap();
        let items = m.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert!(items.is_empty());
    }

    /// Corrupt on disk means start empty, not fail to start.
    #[tokio::test]
    async fn a_corrupt_store_loads_as_empty_rather_than_failing() {
        let path = temp("corrupt");
        std::fs::write(&path, "{ not json").unwrap();

        let m = FileMemory::load(path.clone());
        let items = m.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert!(items.is_empty());
    }

    /// The reason for JSONL. A process killed mid-append leaves a partial final
    /// line; every complete line before it must still be recalled. The
    /// equivalent damage to a single JSON array destroys the whole history,
    /// because a truncated array does not parse at all.
    #[tokio::test]
    async fn a_torn_final_line_costs_only_that_line() {
        let path = temp("torn");

        let m = FileMemory::load(path.clone());
        record_user(&m, "first").await;
        record_user(&m, "second").await;
        drop(m);

        // Simulate the interrupted write: a record that never finished.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"user\":\"third but tru").unwrap();
        drop(f);

        let after = FileMemory::load(path.clone());
        let items = after.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert_eq!(items.len(), 2, "complete lines must survive a torn tail");
        assert_eq!(items[0].content, "first");
        assert_eq!(items[1].content, "second");
    }

    /// A header from a version this build does not know must not be read as if
    /// it were v1 -- the fields could mean anything.
    #[tokio::test]
    async fn a_future_schema_version_loads_as_empty() {
        let path = temp("future");
        std::fs::write(
            &path,
            "{\"schema\":\"uia-conversation\",\"v\":99}\n{\"user\":\"hello\"}\n",
        )
        .unwrap();

        let m = FileMemory::load(path.clone());
        let items = m.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert!(items.is_empty());
    }

    /// Every write path must leave a file the loader accepts, header included.
    #[tokio::test]
    async fn the_file_starts_with_a_versioned_header() {
        let path = temp("header");

        let m = FileMemory::load(path.clone());
        record_user(&m, "hello").await;

        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let first = raw.lines().next().unwrap();
        let h: Header = serde_json::from_str(first).unwrap();
        assert_eq!(h.schema, SCHEMA);
        assert_eq!(h.v, SCHEMA_VERSION);
    }

    /// Recall is capped so a long history cannot grow the setup prompt without
    /// bound -- it is paid for on every connect, and connects are now routine.
    #[tokio::test]
    async fn recall_is_capped_to_the_most_recent_exchanges() {
        let path = temp("cap");

        let m = FileMemory::load(path.clone());
        for i in 0..(RECALL_LIMIT + 5) {
            record_user(&m, &format!("turn {i}")).await;
        }
        let items = m.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert_eq!(items.len(), RECALL_LIMIT);
        assert_eq!(items[0].content, "turn 5", "oldest must have been dropped");
    }

    /// The append-only drawback, handled: the log is allowed to overshoot and
    /// is then compacted back, so the file cannot grow without bound and the
    /// oldest exchanges really are gone from disk, not merely hidden.
    #[tokio::test]
    async fn the_log_is_compacted_once_it_passes_the_threshold() {
        let path = temp("compact");

        let m = FileMemory::load(path.clone());
        for i in 0..(COMPACT_THRESHOLD + 2) {
            record_user(&m, &format!("turn {i}")).await;
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let lines = raw.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(
            lines <= RETAIN_LIMIT + 2,
            "expected a compacted log, got {lines} lines"
        );
        assert!(
            !raw.contains("\"turn 0\""),
            "compaction must actually drop the oldest exchanges from disk"
        );
        assert!(
            raw.contains(&format!("\"turn {}\"", COMPACT_THRESHOLD + 1)),
            "the newest exchange must survive the compaction that consumed it"
        );
    }

    /// Compaction is the one place the whole file is replaced, so it is the one
    /// place a crash could lose everything. Reloading across it proves the
    /// rename left a complete, parseable document.
    #[tokio::test]
    async fn history_survives_a_compaction() {
        let path = temp("survive");

        let m = FileMemory::load(path.clone());
        for i in 0..(COMPACT_THRESHOLD + 1) {
            record_user(&m, &format!("turn {i}")).await;
        }
        drop(m);

        let after = FileMemory::load(path.clone());
        let items = after.recall(&ctx(), None).await.unwrap();

        std::fs::remove_file(&path).ok();
        assert_eq!(items.len(), RECALL_LIMIT);
        assert_eq!(
            items[RECALL_LIMIT - 1].content,
            format!("turn {}", COMPACT_THRESHOLD)
        );
    }

    /// Compaction must not leave its scratch file lying beside the real one.
    #[tokio::test]
    async fn compaction_leaves_no_temp_file_behind() {
        let path = temp("notmp");

        let m = FileMemory::load(path.clone());
        for i in 0..(COMPACT_THRESHOLD + 1) {
            record_user(&m, &format!("turn {i}")).await;
        }

        let tmp = m.temp_path();
        let leftover = tmp.exists();
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&tmp).ok();
        assert!(!leftover, "left a temp file at {tmp:?}");
    }

    /// Guards the atomic rewrite. `compact` is the one place the whole file is
    /// replaced, so if it staged into the destination itself, a crash mid-write
    /// would destroy the entire history -- the exact failure JSONL was chosen
    /// to avoid. That the staging file is distinct, and a sibling so the rename
    /// stays within one filesystem, is all of that property that can be checked
    /// without killing the process mid-write.
    #[test]
    fn compaction_stages_into_a_sibling_file_not_the_destination() {
        let path = PathBuf::from("/somewhere/uia-conversation.jsonl");
        let m = FileMemory::load(path.clone());
        let tmp = m.temp_path();

        assert_ne!(tmp, path, "staging into the destination is not atomic");
        assert_eq!(
            tmp.parent(),
            path.parent(),
            "a rename across directories may cross a filesystem and fail"
        );
    }

    /// Closes the one gap the earlier mutation run left open: deleting the
    /// `sync_all` before the rename passed every test, because a flush leaves
    /// no trace in the file it produces.
    ///
    /// Asserting the plan works only because `replace_durably` is driven by it
    /// -- dropping `Sync` here is the sole way to stop the sync happening. It
    /// does not prove durability, which needs a real power cut no in-process
    /// test can stage. It does mean removing or reordering a step cannot pass
    /// silently.
    #[test]
    fn the_data_is_synced_before_the_rename_publishes_it() {
        let pos = |s: ReplaceStep| REPLACE_PLAN.iter().position(|x| *x == s);

        let write = pos(ReplaceStep::Write).expect("nothing would be staged");
        let sync = pos(ReplaceStep::Sync).expect(
            "without a sync, a power loss after the rename leaves the \
             destination present but empty -- which loads as empty history, \
             silently discarding the whole conversation",
        );
        let rename = pos(ReplaceStep::Rename).expect("nothing would be published");

        assert!(write < sync, "cannot sync bytes that were never written");
        assert!(
            sync < rename,
            "syncing after the rename publishes a file whose contents may still \
             be only in the page cache"
        );

        #[cfg(unix)]
        {
            let sync_dir = pos(ReplaceStep::SyncDir).expect(
                "on POSIX a rename is a directory change, no more durable than \
                 the file was: the new name can be lost though the data survived",
            );
            assert!(rename < sync_dir, "nothing to sync before the rename");
        }
    }

    /// The plan is only worth asserting if executing it does the job.
    #[test]
    fn a_durable_replace_lands_the_bytes_at_the_destination() {
        let dir = std::env::temp_dir().join(format!("uia-mem-steps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("uia-conversation.jsonl");
        let tmp = dest.with_extension("jsonl.tmp");

        replace_durably(&tmp, &dest, b"payload\n").unwrap();
        let landed = std::fs::read_to_string(&dest).unwrap();
        let leftover = tmp.exists();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(landed, "payload\n");
        assert!(!leftover, "the staging file must be gone after a rename");
    }

    /// Replacing a file that already has contents is the real case -- every
    /// compaction after the first one.
    #[test]
    fn a_durable_replace_overwrites_an_existing_destination() {
        let dir = std::env::temp_dir().join(format!("uia-mem-overwrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("uia-conversation.jsonl");
        std::fs::write(&dest, "stale and much longer than what replaces it\n").unwrap();
        let tmp = dest.with_extension("jsonl.tmp");

        replace_durably(&tmp, &dest, b"fresh\n").unwrap();
        let landed = std::fs::read_to_string(&dest).unwrap();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(landed, "fresh\n", "no trace of the previous contents");
    }

    /// A failed replace must not leave its scratch file behind, or the next
    /// attempt fails for a reason that has nothing to do with what went wrong.
    /// A directory standing where the destination should be makes the rename
    /// fail without any of the earlier steps failing first.
    #[test]
    fn a_failed_replace_cleans_up_its_temp_file() {
        let dir = std::env::temp_dir().join(format!("uia-mem-failclean-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("uia-conversation.jsonl");
        std::fs::create_dir_all(&dest).unwrap();
        let tmp = dest.with_extension("jsonl.tmp");

        let result = replace_durably(&tmp, &dest, b"payload\n");

        let leftover = tmp.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(result.is_err(), "renaming onto a directory must fail");
        assert!(!leftover, "left a temp file at {tmp:?}");
    }

    #[test]
    fn the_store_is_a_sibling_of_the_config_file() {
        assert_eq!(
            FileMemory::path_for(Path::new("/somewhere/uia.toml")),
            Path::new("/somewhere/uia-conversation.jsonl")
        );
    }
}

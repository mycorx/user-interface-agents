// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Fan one `ToolExecutor` out over several MCP servers.
//!
//! `SessionDeps` holds exactly one executor, but a config may list several
//! servers (`uia.example.toml` ships two). S8 built the per-server client and
//! the `server.tool` namespace; nothing until S14 owned turning N servers into
//! the one executor a session consumes.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use uia_core::tools::{ToolDescriptor, ToolError, ToolExecutor, ToolResult};

/// Routes a namespaced tool call to the server that owns the namespace.
///
/// Dispatch is by the `server` half of `server.tool`, never by asking each
/// server in turn: two servers may expose the same bare tool name, and "first
/// one that answers" would silently call the wrong one.
pub struct McpRouter {
    servers: Vec<(String, Arc<dyn ToolExecutor>)>,
    list_deadline: Duration,
    /// Optional so a router built without one — every test that predates
    /// this, and any future caller with nowhere to put the outcomes — keeps
    /// working unchanged.
    listing_observer: Option<Arc<dyn ListingObserver>>,
}

/// How long one server gets to answer `list_tools` before it is skipped.
///
/// Generous enough that a cold stdio server still makes it in, short enough
/// that a wedged one cannot dominate a persona switch — `list_tools` runs
/// inside `Session::connect`, which the rotation path calls while the user is
/// waiting on the result.
pub const DEFAULT_LIST_DEADLINE: Duration = Duration::from_secs(2);

/// What one server contributed to a single `list_tools` call.
///
/// Reported per server rather than merged, because "the session has no
/// weather tools" and "the weather server timed out" are different facts and
/// only the second one tells a human what to do about it.
#[derive(Debug, Clone, PartialEq)]
pub enum ListingOutcome {
    /// It answered. `tools` may be zero — a server is free to declare none,
    /// and that is still a session without its tools.
    Listed { tools: usize },
    /// It answered with an error, or the transport under it did.
    Failed { message: String },
    /// It never answered, and the deadline ran out.
    TimedOut { after: Duration },
}

/// Where `list_tools` sends its per-server outcomes.
///
/// A trait rather than a concrete handle so `uia-mcp` stays ignorant of the
/// app: the router knows what happened, the app decides what that means and
/// where it is shown. `Send + Sync` because the outcomes are produced inside
/// concurrent per-server futures.
pub trait ListingObserver: Send + Sync {
    fn record(&self, server: &str, outcome: ListingOutcome);
}

impl McpRouter {
    pub fn new(servers: Vec<(String, Arc<dyn ToolExecutor>)>) -> Self {
        Self {
            servers,
            list_deadline: DEFAULT_LIST_DEADLINE,
            listing_observer: None,
        }
    }

    pub fn with_list_deadline(mut self, d: Duration) -> Self {
        self.list_deadline = d;
        self
    }

    pub fn with_listing_observer(mut self, observer: Arc<dyn ListingObserver>) -> Self {
        self.listing_observer = Some(observer);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    fn route(&self, name: &str) -> Option<&Arc<dyn ToolExecutor>> {
        let (server, _) = name.split_once('.')?;
        self.servers
            .iter()
            .find(|(n, _)| n == server)
            .map(|(_, e)| e)
    }
}

#[async_trait]
impl ToolExecutor for McpRouter {
    /// Merges every server's tools. One unreachable server must not blank the
    /// whole tool list: a session with three servers and one down is still a
    /// session with two servers' tools, and silently declaring none is how an
    /// assistant ends up claiming it "can't do that" for no visible reason.
    ///
    /// Concurrent, and per-server time-boxed, because both of those are the
    /// same requirement seen twice: this runs inside `Session::connect`, which
    /// the persona rotation path calls with the user waiting on the answer. A
    /// sequential walk charges that user the *sum* of every server's latency,
    /// and an unbounded await lets a single wedged server hold the switch open
    /// forever. A server that is merely down already answered promptly, with an
    /// error; the deadline is for the one that never answers at all.
    ///
    /// `join_all` preserves input order, so the merged list stays in
    /// configuration order rather than in whatever order the servers replied.
    async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
        let per_server = self.servers.iter().map(|(name, exec)| async move {
            let (tools, outcome) =
                match tokio::time::timeout(self.list_deadline, exec.list_tools()).await {
                    Ok(Ok(tools)) => {
                        let listed = ListingOutcome::Listed { tools: tools.len() };
                        (tools, listed)
                    }
                    Ok(Err(e)) => {
                        eprintln!("mcp: skipping a server that failed to list tools: {e}");
                        (
                            Vec::new(),
                            ListingOutcome::Failed {
                                message: e.to_string(),
                            },
                        )
                    }
                    Err(_) => {
                        eprintln!(
                            "mcp: skipping {name}, which did not list its tools within {:?}",
                            self.list_deadline
                        );
                        (
                            Vec::new(),
                            ListingOutcome::TimedOut {
                                after: self.list_deadline,
                            },
                        )
                    }
                };
            // stderr still gets the skips, for a run with no Settings panel
            // open to read them.
            if let Some(observer) = &self.listing_observer {
                observer.record(name, outcome);
            }
            tools
        });
        Ok(futures::future::join_all(per_server)
            .await
            .into_iter()
            .flatten()
            .collect())
    }

    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        deadline: Duration,
    ) -> Result<ToolResult, ToolError> {
        match self.route(name) {
            Some(exec) => exec.execute(name, args, deadline).await,
            // Data for the model, not a dead session — `Session` turns a
            // `ToolError` into a spoken error and carries on.
            None => Err(ToolError::Transport(format!(
                "no MCP server owns the tool {name:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked to run, so routing can be asserted directly
    /// rather than inferred from a return value.
    struct SpyExecutor {
        tool: String,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ToolExecutor for SpyExecutor {
        async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
            Ok(vec![ToolDescriptor {
                name: self.tool.clone(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                requires_confirmation: false,
            }])
        }
        async fn execute(
            &self,
            name: &str,
            _args: serde_json::Value,
            _deadline: Duration,
        ) -> Result<ToolResult, ToolError> {
            self.calls.lock().unwrap().push(name.to_string());
            Ok(ToolResult::ok(self.tool.clone()))
        }
    }

    struct BrokenExecutor;

    #[async_trait]
    impl ToolExecutor for BrokenExecutor {
        async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
            Err(ToolError::Transport("server is down".into()))
        }
        async fn execute(
            &self,
            _name: &str,
            _args: serde_json::Value,
            _deadline: Duration,
        ) -> Result<ToolResult, ToolError> {
            Err(ToolError::Transport("server is down".into()))
        }
    }

    /// Connected, accepting requests, and never answering. This is the failure
    /// `BrokenExecutor` does not cover: a server that is *down* returns an
    /// error promptly, while one that is wedged holds the await open forever.
    struct HungExecutor;

    #[async_trait]
    impl ToolExecutor for HungExecutor {
        async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
            std::future::pending().await
        }
        async fn execute(
            &self,
            _name: &str,
            _args: serde_json::Value,
            _deadline: Duration,
        ) -> Result<ToolResult, ToolError> {
            std::future::pending().await
        }
    }

    /// Answers, but takes its time about it.
    struct SlowExecutor {
        tool: String,
        delay: Duration,
    }

    #[async_trait]
    impl ToolExecutor for SlowExecutor {
        async fn list_tools(&self) -> Result<Vec<ToolDescriptor>, ToolError> {
            tokio::time::sleep(self.delay).await;
            Ok(vec![ToolDescriptor {
                name: self.tool.clone(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                requires_confirmation: false,
            }])
        }
        async fn execute(
            &self,
            _name: &str,
            _args: serde_json::Value,
            _deadline: Duration,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok(self.tool.clone()))
        }
    }

    fn spy(tool: &str) -> (Arc<dyn ToolExecutor>, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(SpyExecutor {
                tool: tool.into(),
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }

    /// Collects everything the router reported, in the order it was told.
    ///
    /// The app's real observer writes into a shared map that Settings reads;
    /// this one keeps the sequence so a test can assert *what* was reported
    /// as well as for which server.
    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<(String, ListingOutcome)>>);

    impl ListingObserver for RecordingObserver {
        fn record(&self, server: &str, outcome: ListingOutcome) {
            self.0.lock().unwrap().push((server.to_string(), outcome));
        }
    }

    impl RecordingObserver {
        fn outcomes(&self) -> Vec<(String, ListingOutcome)> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn a_call_goes_to_the_server_that_owns_the_namespace() {
        let (clock, clock_calls) = spy("clock.now");
        let (files, files_calls) = spy("files.now");
        let router = McpRouter::new(vec![("clock".into(), clock), ("files".into(), files)]);

        router
            .execute("files.now", serde_json::json!({}), Duration::from_secs(1))
            .await
            .unwrap();

        // Both servers expose a bare `now`; only the namespace disambiguates.
        assert!(clock_calls.lock().unwrap().is_empty());
        assert_eq!(files_calls.lock().unwrap().as_slice(), ["files.now"]);
    }

    #[tokio::test]
    async fn every_servers_tools_are_merged_into_one_list() {
        let (clock, _) = spy("clock.now");
        let (files, _) = spy("files.read");
        let router = McpRouter::new(vec![("clock".into(), clock), ("files".into(), files)]);

        let names: Vec<String> = router
            .list_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["clock.now", "files.read"]);
    }

    #[tokio::test]
    async fn one_dead_server_does_not_blank_the_whole_tool_list() {
        let (clock, _) = spy("clock.now");
        let router = McpRouter::new(vec![
            ("clock".into(), clock),
            ("dead".into(), Arc::new(BrokenExecutor)),
        ]);

        let tools = router.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1, "the healthy server's tools must survive");
        assert_eq!(tools[0].name, "clock.now");
    }

    #[tokio::test]
    async fn one_hung_server_does_not_stall_the_whole_tool_list() {
        // The dead-server case above is already handled, because a dead server
        // answers. A wedged one never does, and `list_tools` sits on
        // `Session::connect`'s critical path — which a persona switch runs
        // through while the user waits.
        let (clock, _) = spy("clock.now");
        let router = McpRouter::new(vec![
            ("clock".into(), clock),
            ("hung".into(), Arc::new(HungExecutor)),
        ])
        .with_list_deadline(Duration::from_millis(100));

        let tools = tokio::time::timeout(Duration::from_secs(5), router.list_tools())
            .await
            .expect("list_tools must give up on a server that never answers")
            .unwrap();

        assert_eq!(tools.len(), 1, "the healthy server's tools must survive");
        assert_eq!(tools[0].name, "clock.now");
    }

    #[tokio::test]
    async fn servers_are_asked_concurrently_rather_than_one_after_another() {
        // Sequentially this is 600ms; concurrently it is 300ms. The assertion
        // is deliberately loose — it is proving the shape, not the schedule.
        let delay = Duration::from_millis(300);
        let router = McpRouter::new(vec![
            (
                "a".into(),
                Arc::new(SlowExecutor {
                    tool: "a.one".into(),
                    delay,
                }) as Arc<dyn ToolExecutor>,
            ),
            (
                "b".into(),
                Arc::new(SlowExecutor {
                    tool: "b.two".into(),
                    delay,
                }),
            ),
        ]);

        let start = std::time::Instant::now();
        let tools = router.list_tools().await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(tools.len(), 2, "both servers must still be represented");
        assert!(
            elapsed < delay * 2,
            "asking two {delay:?} servers took {elapsed:?}, which is the \
             sequential cost, not the concurrent one"
        );
    }

    #[tokio::test]
    async fn an_unowned_namespace_is_an_error_the_model_can_be_told_about() {
        let router = McpRouter::new(vec![]);
        let err = router
            .execute("ghost.tool", serde_json::json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost.tool"), "got {err}");
    }

    #[tokio::test]
    async fn a_server_that_lists_its_tools_is_reported_as_listed() {
        let (clock, _) = spy("clock.now");
        let seen = Arc::new(RecordingObserver::default());
        let router = McpRouter::new(vec![("clock".into(), clock)])
            .with_listing_observer(Arc::clone(&seen) as Arc<dyn ListingObserver>);

        router.list_tools().await.unwrap();

        assert_eq!(
            seen.outcomes(),
            vec![("clock".to_string(), ListingOutcome::Listed { tools: 1 })]
        );
    }

    #[tokio::test]
    async fn a_server_whose_listing_fails_is_reported_with_the_reason() {
        let seen = Arc::new(RecordingObserver::default());
        let router = McpRouter::new(vec![(
            "dead".into(),
            Arc::new(BrokenExecutor) as Arc<dyn ToolExecutor>,
        )])
        .with_listing_observer(Arc::clone(&seen) as Arc<dyn ListingObserver>);

        router.list_tools().await.unwrap();

        let outcomes = seen.outcomes();
        assert_eq!(outcomes.len(), 1, "got {outcomes:?}");
        let (name, outcome) = &outcomes[0];
        assert_eq!(name, "dead");
        match outcome {
            // The reason has to travel, not just the fact: "no tools" without
            // it is the same silence this reporting exists to end.
            ListingOutcome::Failed { message } => {
                assert!(message.contains("server is down"), "got {message:?}")
            }
            other => panic!("expected a failed listing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_server_that_never_answers_is_reported_as_timed_out() {
        let deadline = Duration::from_millis(50);
        let seen = Arc::new(RecordingObserver::default());
        let router = McpRouter::new(vec![(
            "hung".into(),
            Arc::new(HungExecutor) as Arc<dyn ToolExecutor>,
        )])
        .with_list_deadline(deadline)
        .with_listing_observer(Arc::clone(&seen) as Arc<dyn ListingObserver>);

        router.list_tools().await.unwrap();

        assert_eq!(
            seen.outcomes(),
            vec![(
                "hung".to_string(),
                ListingOutcome::TimedOut { after: deadline }
            )]
        );
    }

    #[tokio::test]
    async fn one_server_failing_does_not_cost_the_others_their_report() {
        // The mirror of `one_dead_server_does_not_blank_the_whole_tool_list`:
        // the healthy server keeps its tools, and both keep their outcome.
        let (clock, _) = spy("clock.now");
        let seen = Arc::new(RecordingObserver::default());
        let router = McpRouter::new(vec![
            ("clock".into(), clock),
            (
                "dead".into(),
                Arc::new(BrokenExecutor) as Arc<dyn ToolExecutor>,
            ),
        ])
        .with_listing_observer(Arc::clone(&seen) as Arc<dyn ListingObserver>);

        router.list_tools().await.unwrap();

        // Sorted, not in call order: the servers are asked concurrently, so
        // which one reports first is a schedule, not a promise.
        let mut reported: Vec<String> = seen.outcomes().into_iter().map(|(n, _)| n).collect();
        reported.sort();
        assert_eq!(reported, vec!["clock", "dead"]);
    }
}

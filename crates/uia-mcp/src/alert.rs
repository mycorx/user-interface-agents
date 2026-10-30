// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The alert model: what an MCP server's alert feed contains, how it is parsed
//! and which entries count as new. Pure and free of `rmcp` types; the listener
//! in `alert_listener` is the only part that talks to a server.
//!
//! An alert feed is a resource with the MIME type [`ALERT_FEED_MIME`] whose
//! text is `{"alerts": [<alert>, ...]}`, newest last.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, SystemTime};

use chrono::DateTime;
use serde::Serialize;
use serde_json::Value;

pub const ALERT_FEED_MIME: &str = "application/vnd.uia.alerts+json";

const SUMMARY_CAP: usize = 200;
const DETAILS_CAP: usize = 500;
const ACTION_LABEL_CAP: usize = 24;
/// Short labels and identifiers; not in the spec's list, capped so a hostile
/// server cannot make UIA hold an unbounded string per alert.
const SHORT_FIELD_CAP: usize = 200;
const TITLE_CAP: usize = 100;

/// Remembered dedupe keys per server.
const SEEN_CAP: usize = 256;
/// Entries created this recently survive the baseline read.
const BASELINE_GRACE: Duration = Duration::from_secs(60);
/// How far ahead of our clock a `created_at` may be and still count as fresh.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(60);
const RATE_LIMIT: usize = 30;
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Whether a `resources/list` entry's MIME type marks it as an alert feed.
pub fn is_alert_feed(mime_type: Option<&str>) -> bool {
    mime_type.is_some_and(|m| m.trim().eq_ignore_ascii_case(ALERT_FEED_MIME))
}

/// A button the server asked for: a label and an `https` link.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AlertAction {
    pub label: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Alert {
    pub server: String,
    pub title: Option<String>,
    pub summary: String,
    pub details: Option<String>,
    pub kind: Option<String>,
    pub id: Option<String>,
    pub stage: Option<String>,
    pub created_at: Option<String>,
    pub action: Option<AlertAction>,
}

fn capped(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

fn string_field(entry: &serde_json::Map<String, Value>, name: &str, cap: usize) -> Option<String> {
    let text = entry.get(name)?.as_str()?;
    (!text.trim().is_empty()).then(|| capped(text, cap))
}

fn parse_action(value: &Value) -> Option<AlertAction> {
    let object = value.as_object()?;
    let label = string_field(object, "label", ACTION_LABEL_CAP)?;
    let raw = object.get("url")?.as_str()?;
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return None;
    }
    Some(AlertAction {
        label,
        url: parsed.to_string(),
    })
}

fn parse_entry(server: &str, value: &Value) -> Option<Alert> {
    let entry = value.as_object()?;
    let summary = string_field(entry, "summary", SUMMARY_CAP)?;
    Some(Alert {
        server: server.to_string(),
        title: string_field(entry, "title", TITLE_CAP),
        summary,
        details: string_field(entry, "details", DETAILS_CAP),
        kind: string_field(entry, "kind", SHORT_FIELD_CAP),
        id: string_field(entry, "id", SHORT_FIELD_CAP),
        stage: string_field(entry, "stage", SHORT_FIELD_CAP),
        created_at: string_field(entry, "created_at", SHORT_FIELD_CAP),
        action: entry.get("action").and_then(parse_action),
    })
}

/// Parse a feed's text. Bad JSON, or JSON without an `alerts` array, yields
/// nothing; entries that are not objects or lack a `summary` are skipped.
pub fn parse_feed(server: &str, text: &str) -> Vec<Alert> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(Value::Array(entries)) = root.get("alerts") else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| parse_entry(server, entry))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Id { id: String, stage: Option<String> },
    Summary(String),
}

impl Key {
    fn of(alert: &Alert) -> Self {
        match &alert.id {
            Some(id) => Key::Id {
                id: id.clone(),
                stage: alert.stage.clone(),
            },
            None => Key::Summary(alert.summary.clone()),
        }
    }
}

#[derive(Default)]
struct ServerState {
    baselined: bool,
    order: VecDeque<Key>,
    seen: HashSet<Key>,
    admitted: VecDeque<SystemTime>,
}

impl ServerState {
    /// True when the key had not been seen; remembers it either way.
    fn remember(&mut self, key: Key) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > SEEN_CAP
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        true
    }

    fn within_rate_limit(&mut self, server: &str, now: SystemTime) -> bool {
        while self
            .admitted
            .front()
            .is_some_and(|t| now.duration_since(*t).is_ok_and(|age| age >= RATE_WINDOW))
        {
            self.admitted.pop_front();
        }
        if self.admitted.len() >= RATE_LIMIT {
            tracing::warn!(server, "dropping an MCP alert: rate limit exceeded");
            return false;
        }
        self.admitted.push_back(now);
        true
    }
}

/// Whether `created_at` is no older than the baseline grace. Missing or
/// unparsable counts as old.
fn is_recent(created_at: Option<&str>, now: SystemTime) -> bool {
    let Some(created) = created_at.and_then(|c| DateTime::parse_from_rfc3339(c).ok()) else {
        return false;
    };
    let created: SystemTime = created.into();
    match now.duration_since(created) {
        Ok(age) => age <= BASELINE_GRACE,
        // Created slightly in the future (clock skew): fresh. Further ahead
        // is a broken clock, and counting it fresh would replay the feed on
        // every start.
        Err(ahead) => ahead.duration() <= MAX_CLOCK_SKEW,
    }
}

/// Decides which alerts of a re-read feed are new. State is per server; time
/// is passed in so behaviour is deterministic.
#[derive(Default)]
pub struct AlertGate {
    servers: HashMap<String, ServerState>,
}

impl AlertGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The first call for a server is a baseline: everything is marked seen
    /// and only entries created within the last 60 s are returned. Later
    /// calls return the entries not seen before. Both are then capped at 30
    /// per server per minute; the excess is dropped.
    pub fn admit(&mut self, alerts: Vec<Alert>, now: SystemTime) -> Vec<Alert> {
        let mut admitted = Vec::new();
        let mut touched = HashSet::new();
        // The feed is newest last. Only the newest `SEEN_CAP` can be remembered,
        // so older entries of a longer feed are ignored rather than re-admitted
        // on every read after eviction.
        let skip = alerts.len().saturating_sub(SEEN_CAP);
        for alert in alerts.into_iter().skip(skip) {
            touched.insert(alert.server.clone());
            let state = self.servers.entry(alert.server.clone()).or_default();
            let baseline = !state.baselined;
            let is_new = state.remember(Key::of(&alert));
            let wanted = is_new && (!baseline || is_recent(alert.created_at.as_deref(), now));
            if wanted && state.within_rate_limit(&alert.server, now) {
                admitted.push(alert);
            }
        }
        // The baseline covers one whole read, not one entry.
        for server in touched {
            self.mark_read(&server);
        }
        admitted
    }

    /// Records that a read happened for `server`. `admit` does this for every
    /// server it sees; the listener also calls it after an empty read, which
    /// `admit` cannot attribute to a server, so that read still ends the baseline.
    pub fn mark_read(&mut self, server: &str) {
        self.servers
            .entry(server.to_string())
            .or_default()
            .baselined = true;
    }

    /// The next read for `server` is a baseline again (after a reconnect).
    pub fn reset_baseline(&mut self, server: &str) {
        if let Some(state) = self.servers.get_mut(server) {
            state.baselined = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_SECS: u64 = 1_800_000_000; // 2027-01-15T08:00:00Z

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    fn rfc3339_ago(seconds: u64) -> String {
        let at = DateTime::from_timestamp((NOW_SECS - seconds) as i64, 0).unwrap();
        at.to_rfc3339()
    }

    fn alert(server: &str, id: Option<&str>, stage: Option<&str>, summary: &str) -> Alert {
        Alert {
            server: server.into(),
            title: None,
            summary: summary.into(),
            details: None,
            kind: None,
            id: id.map(Into::into),
            stage: stage.map(Into::into),
            created_at: None,
            action: None,
        }
    }

    fn aged(mut a: Alert, seconds: u64) -> Alert {
        a.created_at = Some(rfc3339_ago(seconds));
        a
    }

    /// Run the baseline read so later reads are ordinary re-reads.
    fn baselined(server: &str, existing: Vec<Alert>) -> AlertGate {
        let mut gate = AlertGate::new();
        gate.admit(existing, now());
        gate.mark_read(server);
        gate
    }

    #[test]
    fn a_feed_with_one_valid_alert_parses_it() {
        let text = r#"{"alerts":[{"kind":"meeting","id":"e1","stage":"T-5","title":"Sync",
            "summary":"Weekly sync starts in 5 minutes","details":"Room 4",
            "created_at":"2027-01-15T08:00:00Z"}]}"#;
        let alerts = parse_feed("mymy", text);
        assert_eq!(
            alerts,
            vec![Alert {
                server: "mymy".into(),
                title: Some("Sync".into()),
                summary: "Weekly sync starts in 5 minutes".into(),
                details: Some("Room 4".into()),
                kind: Some("meeting".into()),
                id: Some("e1".into()),
                stage: Some("T-5".into()),
                created_at: Some("2027-01-15T08:00:00Z".into()),
                action: None,
            }]
        );
    }

    #[test]
    fn bad_json_yields_no_alerts() {
        assert!(parse_feed("s", "not json").is_empty());
        assert!(parse_feed("s", "[]").is_empty());
        assert!(parse_feed("s", r#"{"alerts":"nope"}"#).is_empty());
        assert!(parse_feed("s", "").is_empty());
    }

    #[test]
    fn entries_without_a_summary_or_not_objects_are_skipped_and_the_rest_kept() {
        let text = r#"{"alerts":["text",7,null,{"title":"no summary"},{"summary":""},
            {"summary":42},{"summary":"kept"}]}"#;
        let alerts = parse_feed("s", text);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].summary, "kept");
    }

    #[test]
    fn an_https_action_is_kept_with_its_label() {
        let text = r#"{"alerts":[{"summary":"x","action":{"label":"Join","url":"https://meet.example.com/abc"}}]}"#;
        let action = parse_feed("s", text)[0].action.clone().unwrap();
        assert_eq!(action.label, "Join");
        assert_eq!(action.url, "https://meet.example.com/abc");
    }

    #[test]
    fn javascript_http_file_and_malformed_action_urls_drop_the_action_but_keep_the_alert() {
        for bad in [
            r#"{"label":"Go","url":"javascript:alert(1)"}"#,
            r#"{"label":"Go","url":"http://example.com/"}"#,
            r#"{"label":"Go","url":"file:///etc/passwd"}"#,
            r#"{"label":"Go","url":"https://"}"#,
            r#"{"label":"Go","url":"not a url"}"#,
            r#"{"label":"Go"}"#,
            r#"{"url":"https://example.com/"}"#,
            r#""https://example.com/""#,
        ] {
            let text = format!(r#"{{"alerts":[{{"summary":"x","action":{bad}}}]}}"#);
            let alerts = parse_feed("s", &text);
            assert_eq!(alerts.len(), 1, "alert kept for {bad}");
            assert_eq!(alerts[0].action, None, "action dropped for {bad}");
        }
    }

    #[test]
    fn over_long_fields_are_truncated_to_their_caps() {
        let text = serde_json::json!({"alerts":[{
            "summary": "s".repeat(300),
            "details": "d".repeat(900),
            "action": {"label": "l".repeat(60), "url": "https://example.com/"},
        }]})
        .to_string();
        let alert = parse_feed("s", &text).remove(0);
        assert_eq!(alert.summary.chars().count(), 200);
        assert_eq!(alert.details.unwrap().chars().count(), 500);
        assert_eq!(alert.action.unwrap().label.chars().count(), 24);
    }

    #[test]
    fn the_mime_check_matches_only_the_alert_feed_type() {
        assert!(is_alert_feed(Some("application/vnd.uia.alerts+json")));
        assert!(!is_alert_feed(Some("application/json")));
        assert!(!is_alert_feed(Some("text/plain")));
        assert!(!is_alert_feed(None));
    }

    #[test]
    fn the_first_read_is_a_baseline_and_shows_nothing_old() {
        let mut gate = AlertGate::new();
        let old = aged(alert("s", Some("a"), None, "old"), 3600);
        let undated = alert("s", Some("b"), None, "undated");
        let mut unparsable = alert("s", Some("c"), None, "garbled");
        unparsable.created_at = Some("yesterday-ish".into());
        assert!(gate.admit(vec![old, undated, unparsable], now()).is_empty());
    }

    #[test]
    fn the_baseline_still_shows_an_entry_created_within_60_seconds() {
        let mut gate = AlertGate::new();
        let old = aged(alert("s", Some("a"), None, "old"), 3600);
        let fresh = aged(alert("s", Some("b"), None, "fresh"), 30);
        let shown = gate.admit(vec![old, fresh.clone()], now());
        assert_eq!(shown, vec![fresh]);
    }

    fn dated_ahead(mut a: Alert, seconds: u64) -> Alert {
        let at = DateTime::from_timestamp((NOW_SECS + seconds) as i64, 0).unwrap();
        a.created_at = Some(at.to_rfc3339());
        a
    }

    #[test]
    fn an_entry_dated_slightly_in_the_future_is_still_shown_by_the_baseline() {
        let mut gate = AlertGate::new();
        let skewed = dated_ahead(alert("s", Some("a"), None, "skewed"), 30);
        assert_eq!(gate.admit(vec![skewed.clone()], now()), vec![skewed]);
    }

    #[test]
    fn an_entry_dated_far_in_the_future_is_not_shown_by_the_baseline() {
        let mut gate = AlertGate::new();
        let broken = dated_ahead(alert("s", Some("a"), None, "broken clock"), 3600);
        assert!(gate.admit(vec![broken], now()).is_empty());
    }

    #[test]
    fn a_reread_with_one_new_entry_returns_only_that_entry() {
        let a = alert("s", Some("a"), Some("T-5"), "a");
        let mut gate = baselined("s", vec![a.clone()]);
        let b = alert("s", Some("b"), Some("T-5"), "b");
        assert_eq!(gate.admit(vec![a.clone(), b.clone()], now()), vec![b]);
        assert!(gate.admit(vec![a], now()).is_empty());
    }

    #[test]
    fn a_new_stage_for_the_same_id_is_new() {
        let t5 = alert("s", Some("a"), Some("T-5"), "in 5");
        let mut gate = baselined("s", vec![t5.clone()]);
        let t2 = alert("s", Some("a"), Some("T-2"), "in 2");
        assert_eq!(gate.admit(vec![t5, t2.clone()], now()), vec![t2]);
    }

    #[test]
    fn dedupe_falls_back_to_summary_without_an_id() {
        let first = alert("s", None, None, "same words");
        let mut gate = baselined("s", vec![]);
        assert_eq!(gate.admit(vec![first.clone()], now()), vec![first.clone()]);
        assert!(gate.admit(vec![first], now()).is_empty());
        let other = alert("s", None, None, "other words");
        assert_eq!(gate.admit(vec![other.clone()], now()), vec![other]);
    }

    #[test]
    fn an_empty_first_read_still_ends_the_baseline() {
        let mut gate = AlertGate::new();
        assert!(gate.admit(vec![], now()).is_empty());
        gate.mark_read("s");
        let late = aged(alert("s", Some("a"), None, "late"), 3600);
        assert_eq!(gate.admit(vec![late.clone()], now()), vec![late]);
    }

    #[test]
    fn the_31st_alert_from_one_server_within_a_minute_is_dropped() {
        let mut gate = baselined("s", vec![]);
        let batch: Vec<Alert> = (0..31)
            .map(|n| alert("s", Some(&format!("id{n}")), None, "x"))
            .collect();
        assert_eq!(gate.admit(batch, now()).len(), 30);
        // Another server has its own budget.
        let mut other = baselined("t", vec![]);
        let batch: Vec<Alert> = (0..30)
            .map(|n| alert("t", Some(&format!("id{n}")), None, "x"))
            .collect();
        assert_eq!(other.admit(batch, now()).len(), 30);
        // The budget refills after a minute.
        let later = now() + Duration::from_secs(61);
        let again = alert("s", Some("later"), None, "x");
        assert_eq!(gate.admit(vec![again.clone()], later), vec![again]);
    }

    #[test]
    fn a_feed_longer_than_the_dedupe_memory_does_not_re_admit_old_entries() {
        let feed: Vec<Alert> = (0..300)
            .map(|n| alert("s", Some(&format!("id{n}")), None, "x"))
            .collect();
        let mut gate = baselined("s", feed.clone());
        assert!(gate.admit(feed.clone(), now()).is_empty());
        assert!(gate.admit(feed, now()).is_empty());
    }

    #[test]
    fn reset_baseline_makes_the_next_read_a_baseline_again() {
        let a = alert("s", Some("a"), None, "a");
        let mut gate = baselined("s", vec![a.clone()]);
        gate.reset_baseline("s");
        let old_new = aged(alert("s", Some("b"), None, "b"), 3600);
        assert!(gate.admit(vec![a, old_new.clone()], now()).is_empty());
        // After that baseline, the next new entry is shown.
        let c = alert("s", Some("c"), None, "c");
        assert_eq!(gate.admit(vec![old_new, c.clone()], now()), vec![c]);
    }
}

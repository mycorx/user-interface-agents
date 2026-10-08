// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! A bounded, redacted tail of a local MCP server's stderr.
//!
//! rmcp's default is to let a child inherit the parent's stderr, so when a
//! server dies at startup the only thing UIA can show is an opaque transport
//! error. We pipe the stream instead, drain it for the life of the process,
//! and keep the last few lines so a startup failure can say what the server
//! itself printed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Most lines kept; older ones are dropped.
const MAX_LINES: usize = 12;
/// Longest stored line, in chars.
const MAX_LINE_CHARS: usize = 300;
/// Longest rendered tail, in chars (the front is dropped, the end survives).
const MAX_RENDER_CHARS: usize = 600;
/// Env values shorter than this are not redacted: they would mangle ordinary
/// words (`true`, `1`, `debug`) and are not credentials in practice.
const MIN_REDACT_LEN: usize = 8;
/// A single stderr line longer than this is cut while reading, so a child that
/// never prints a newline cannot grow our memory without bound.
const MAX_READ_LINE_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct StderrTail {
    redact: Arc<Vec<String>>,
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    /// `redact` is every launch env value; only those of length >= 8 are kept.
    pub(crate) fn new(redact: Vec<String>) -> Self {
        let mut values: Vec<String> = redact
            .into_iter()
            .filter(|v| v.chars().count() >= MIN_REDACT_LEN)
            .collect();
        // Longest first, so a value that contains another is replaced whole.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        values.dedup();
        Self {
            redact: Arc::new(values),
            lines: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Replace every occurrence of every launch env value with `<redacted>`.
    pub(crate) fn redact(&self, s: &str) -> String {
        let mut out = s.to_string();
        for value in self.redact.iter() {
            out = out.replace(value.as_str(), "<redacted>");
        }
        out
    }

    pub(crate) fn push_line(&self, raw: &str) {
        let cleaned = self.redact(&strip_control(raw));
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            return;
        }
        let line: String = trimmed.chars().take(MAX_LINE_CHARS).collect();
        // The lock is only ever held for this push; a poisoned lock (a panic
        // elsewhere) must not stop the drain, so recover the data.
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines.push_back(line);
        while lines.len() > MAX_LINES {
            lines.pop_front();
        }
    }

    /// The captured lines joined with ` | `, at most 600 chars, cut from the
    /// front so the most recent output survives. Empty when nothing was seen.
    pub(crate) fn render(&self) -> String {
        let joined = {
            let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
            lines
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let count = joined.chars().count();
        if count <= MAX_RENDER_CHARS {
            return joined;
        }
        joined.chars().skip(count - MAX_RENDER_CHARS).collect()
    }
}

/// Remove ANSI escape sequences (CSI and OSC) and control characters other
/// than tab.
fn strip_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: ESC [ params... final byte in 0x40..=0x7e
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... terminated by BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // Any other two-byte escape.
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Drain `stderr` until EOF for the life of the child: a full pipe buffer
/// (~64 KB) would otherwise freeze a chatty server. Each line is stored in
/// `tail` and still echoed to our own stderr, redacted and prefixed, as the
/// inherited stream used to be. Reads lossily so a non-UTF8 byte does not end
/// the task, and never panics.
pub(crate) fn spawn_drain(
    stderr: tokio::process::ChildStderr,
    tail: StderrTail,
    server_name: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(drain(stderr, tail, server_name))
}

async fn drain<R: AsyncRead + Unpin>(stderr: R, tail: StderrTail, server_name: String) {
    let mut reader = BufReader::new(stderr);
    let mut line: Vec<u8> = Vec::new();
    let mut overlong = false;
    loop {
        let (consumed, found_newline) = {
            let buf = match reader.fill_buf().await {
                Ok(b) => b,
                Err(_) => break,
            };
            if buf.is_empty() {
                break;
            }
            let (chunk, found) = match buf.iter().position(|b| *b == b'\n') {
                Some(i) => (&buf[..i], true),
                None => (buf, false),
            };
            let room = MAX_READ_LINE_BYTES.saturating_sub(line.len());
            if chunk.len() > room {
                overlong = true;
            }
            line.extend_from_slice(&chunk[..chunk.len().min(room)]);
            (chunk.len() + usize::from(found), found)
        };
        reader.consume(consumed);
        if found_newline {
            emit(&tail, &server_name, &line);
            line.clear();
            overlong = false;
        }
    }
    // A final line with no trailing newline.
    if !line.is_empty() || overlong {
        emit(&tail, &server_name, &line);
    }
}

fn emit(tail: &StderrTail, server_name: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    tail.push_line(&text);
    let echoed = tail.redact(text.trim_end_matches('\r'));
    eprintln!("mcp[{server_name}] {echoed}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail() -> StderrTail {
        StderrTail::new(vec![])
    }

    #[test]
    fn keeps_only_the_last_twelve_lines() {
        let t = tail();
        for i in 0..30 {
            t.push_line(&format!("line-{i}"));
        }
        let r = t.render();
        assert!(r.starts_with("line-18"), "{r}");
        assert!(r.ends_with("line-29"), "{r}");
        assert_eq!(r.matches(" | ").count(), 11);
    }

    #[test]
    fn truncates_a_long_line_to_300_chars() {
        let t = tail();
        t.push_line(&"x".repeat(1000));
        assert_eq!(t.render().chars().count(), 300);
    }

    #[test]
    fn skips_blank_lines() {
        let t = tail();
        t.push_line("");
        t.push_line("   \t ");
        t.push_line("\x1b[0m");
        assert_eq!(t.render(), "");
    }

    #[test]
    fn strips_ansi_and_control_characters() {
        let t = tail();
        t.push_line("\x1b[31mred\x1b[0m\x07 a\u{0}b\tc\r");
        assert_eq!(t.render(), "red ab\tc");
    }

    #[test]
    fn redacts_long_env_values_everywhere_but_not_short_ones() {
        let t = StderrTail::new(vec!["supersecret-value".into(), "true".into()]);
        t.push_line("a supersecret-value b supersecret-value c true");
        assert_eq!(t.render(), "a <redacted> b <redacted> c true");
        assert_eq!(t.redact("x supersecret-value"), "x <redacted>");
    }

    #[test]
    fn render_is_bounded_and_ends_with_the_latest_line() {
        let t = tail();
        for i in 0..30 {
            t.push_line(&format!("{i}-{}", "y".repeat(200)));
        }
        let r = t.render();
        assert!(r.chars().count() <= 600, "{}", r.chars().count());
        assert!(r.ends_with(&format!("29-{}", "y".repeat(200))));
    }

    #[test]
    fn render_of_nothing_is_empty() {
        assert_eq!(tail().render(), "");
    }

    #[tokio::test]
    async fn drain_survives_non_utf8_and_a_final_unterminated_line() {
        let t = tail();
        let data: &[u8] = b"before\n\xff\xfe bad\nafter\nno-newline";
        drain(data, t.clone(), "probe".into()).await;
        let r = t.render();
        assert!(r.contains("before"), "{r}");
        assert!(r.contains("bad"), "{r}");
        assert!(r.contains("after"), "{r}");
        assert!(r.ends_with("no-newline"), "{r}");
    }
}

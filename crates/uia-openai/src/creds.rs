// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Credentials: the long-lived key, and the ephemeral secret minted from it.
//!
//! The long-lived key authenticates exactly one request — minting a client
//! secret. It never touches the peer connection, never reaches the WebView, and
//! never appears in a log line or an error string.

use serde_json::Value;
use std::path::PathBuf;
use uia_core::engine::EngineError;

/// Where the long-lived key is looked for when `OPENAI_API_KEY` is unset.
/// Gitignored (`*.key` is ignored repo-wide).
pub fn default_key_files() -> Vec<PathBuf> {
    ["gpt-api.key", "../gpt-api.key", "../../gpt-api.key"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Environment first, key file second. The caller reads the environment so this
/// stays a pure function — `std::env::set_var` is `unsafe` in edition 2024 and
/// racy under the test harness's thread pool.
pub fn resolve_api_key(
    env_value: Option<String>,
    key_files: &[PathBuf],
) -> Result<String, EngineError> {
    if let Some(k) = env_value {
        let k = k.trim();
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    for path in key_files {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    // Auth errors are terminal: a missing key is not something backoff fixes.
    Err(EngineError::Auth(format!(
        "no OPENAI_API_KEY and no readable key file (tried {})",
        key_files
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Pull the ephemeral secret out of a `POST /v1/realtime/client_secrets` reply.
///
/// The failure message is deliberately fixed text: an upstream error body can
/// echo the key back, and interpolating it would leak the key into the logs.
pub fn parse_client_secret(body: &Value) -> Result<String, EngineError> {
    body["value"]
        .as_str()
        .or_else(|| body["client_secret"]["value"].as_str())
        .map(str::to_string)
        .ok_or_else(|| EngineError::Auth("mint response carried no client secret".to_string()))
}

/// Mint an ephemeral client secret. The only use of the long-lived key.
pub async fn mint_client_secret(
    http: &reqwest::Client,
    api_key: &str,
    model: &str,
) -> Result<String, EngineError> {
    let resp = http
        .post("https://api.openai.com/v1/realtime/client_secrets")
        .bearer_auth(api_key)
        .json(&serde_json::json!({"session": {"type": "realtime", "model": model}}))
        .send()
        .await
        .map_err(|e| EngineError::Transport(e.to_string()))?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| EngineError::Protocol(e.to_string()))?;

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(EngineError::Auth(format!(
            "mint rejected the key ({status})"
        )));
    }
    parse_client_secret(&body)
}

/// Convenience: the key file path is unused here, but kept so callers outside
/// this crate need not know the search order.
pub fn resolve_api_key_from_env() -> Result<String, EngineError> {
    resolve_api_key(std::env::var("OPENAI_API_KEY").ok(), &default_key_files())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::engine::EngineError;

    #[test]
    fn the_environment_key_wins_over_the_key_file() {
        let dir = tempdir();
        let file = dir.join("gpt-api.key");
        std::fs::write(&file, "from-file").unwrap();
        assert_eq!(
            resolve_api_key(Some("from-env".into()), &[file]).unwrap(),
            "from-env"
        );
    }

    #[test]
    fn a_key_file_is_the_fallback_when_the_environment_is_unset() {
        let dir = tempdir();
        let file = dir.join("gpt-api.key");
        std::fs::write(&file, "  sk-from-file\n").unwrap();
        // Trailing newline from `echo` must not travel into an auth header.
        assert_eq!(resolve_api_key(None, &[file]).unwrap(), "sk-from-file");
    }

    #[test]
    fn a_blank_environment_key_falls_through_to_the_file() {
        let dir = tempdir();
        let file = dir.join("gpt-api.key");
        std::fs::write(&file, "sk-from-file").unwrap();
        assert_eq!(
            resolve_api_key(Some("   ".into()), &[file]).unwrap(),
            "sk-from-file"
        );
    }

    #[test]
    fn no_key_anywhere_is_a_terminal_auth_error() {
        let missing = tempdir().join("absent.key");
        let err = resolve_api_key(None, &[missing]).unwrap_err();
        assert!(matches!(err, EngineError::Auth(_)));
        assert!(err.is_terminal(), "a missing key must never be retried");
    }

    #[test]
    fn the_ephemeral_secret_is_read_from_the_mint_response() {
        let body = serde_json::json!({
            "value": "ek_abcdefghijklmnopqrstuvwxyz0123456",
            "expires_at": 1786923028,
            "session": {"type": "realtime"}
        });
        assert_eq!(
            parse_client_secret(&body).unwrap(),
            "ek_abcdefghijklmnopqrstuvwxyz0123456"
        );
    }

    #[test]
    fn the_nested_client_secret_shape_is_also_accepted() {
        let body = serde_json::json!({"client_secret": {"value": "ek_nested"}});
        assert_eq!(parse_client_secret(&body).unwrap(), "ek_nested");
    }

    #[test]
    fn a_mint_response_carrying_no_secret_is_an_auth_error() {
        let body = serde_json::json!({"error": {"message": "Incorrect API key provided"}});
        let err = parse_client_secret(&body).unwrap_err();
        assert!(matches!(err, EngineError::Auth(_)));
        assert!(err.is_terminal());
    }

    #[test]
    fn a_failed_mint_never_puts_the_key_in_the_error() {
        // The long-lived key must not reach a log line through an error string.
        let body =
            serde_json::json!({"error": {"message": "Incorrect API key provided: sk-live-SECRET"}});
        let err = parse_client_secret(&body).unwrap_err();
        assert!(
            !err.to_string().contains("sk-live-SECRET"),
            "leaked a key: {err}"
        );
    }

    /// Tests run on parallel threads, and the clock only ticks in microseconds
    /// on macOS, so a name built from the time alone is shared by tests that
    /// start together — and they then truncate each other's key file.
    #[test]
    fn scratch_directories_are_unique_when_threads_start_together() {
        const THREADS: usize = 16;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    tempdir()
                })
            })
            .collect();
        let dirs: std::collections::HashSet<_> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            dirs.len(),
            THREADS,
            "two threads were handed the same directory"
        );
    }

    /// A unique scratch directory; avoids a dev-dependency for four tests.
    ///
    /// Named from the process id and a counter, never the clock: parallel test
    /// threads can read the same tick (macOS's is a microsecond), and two tests
    /// sharing a directory share `gpt-api.key`, so one truncates it while the
    /// other reads it.
    fn tempdir() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("uia-openai-creds-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

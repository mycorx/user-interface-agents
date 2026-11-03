// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Where the app's own messages go.
//!
//! Everything the app has to say goes to stderr: its `eprintln!` lines, and
//! the `tracing` events the MCP client emits (for example why a server
//! produces no alerts). A packaged build started from the Dock, Start menu
//! or a desktop launcher has no terminal, so that stderr goes nowhere and a
//! failure leaves no trace. In that case stderr is sent to a log file in the
//! per-user directory instead. A checkout, or any run with a terminal
//! attached, keeps printing to the terminal.
//!
//! This module must not depend on `tauri`, like `paths`.

use crate::paths::ConfigLocation;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use tracing_subscriber::filter::{LevelFilter, Targets};

/// The log file's name, inside [`LOG_DIR_NAME`].
pub const LOG_FILE: &str = "uia.log";

/// The directory under the per-user config directory that holds the log.
pub const LOG_DIR_NAME: &str = "logs";

/// A log larger than this at startup is moved aside to `uia.log.1`
/// (replacing an older one), so the file never grows without limit.
pub const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Overrides the default filter, in `tracing` target syntax, for example
/// `UIA_LOG=debug` or `UIA_LOG=warn,uia_mcp=debug`.
pub const LOG_FILTER_ENV: &str = "UIA_LOG";

/// Where a packaged build writes its log: `<per-user dir>/logs/uia.log`.
pub fn log_file_path(location: &ConfigLocation) -> PathBuf {
    location.config_dir().join(LOG_DIR_NAME).join(LOG_FILE)
}

/// Whether stderr should go to the log file: only for a packaged install
/// with no terminal to print to.
pub fn should_redirect(location: &ConfigLocation, stderr_is_terminal: bool) -> bool {
    matches!(location, ConfigLocation::User(_)) && !stderr_is_terminal
}

/// Move a log over `limit` bytes aside to `<name>.1`. A missing log is fine.
pub fn rotate_if_large(path: &Path, limit: u64) -> io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > limit => std::fs::rename(path, rotated_path(path)),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// What is logged: warnings from everything, plus the app's own crates at
/// info, unless [`LOG_FILTER_ENV`] says otherwise. An unparsable override
/// falls back to the default rather than silencing the log.
pub fn filter(env_override: Option<&str>) -> Targets {
    if let Some(parsed) = env_override.and_then(|s| s.parse::<Targets>().ok()) {
        return parsed;
    }
    Targets::new().with_default(LevelFilter::WARN).with_targets(
        [
            "uia_app",
            "uia_audio",
            "uia_core",
            "uia_foundry",
            "uia_mcp",
            "uia_nova",
            "uia_openai",
        ]
        .map(|target| (target, LevelFilter::INFO)),
    )
}

/// Send `tracing` events to stderr, which [`redirect_stderr_to`] may have
/// pointed at the log file. Without this the MCP client's events are
/// dropped. Calling it twice keeps the first subscriber.
pub fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let env = std::env::var(LOG_FILTER_ENV).ok();
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(false),
        )
        .with(filter(env.as_deref()))
        .try_init();
}

/// Open the log for appending (private to the user on Unix), rotating it
/// first if it is large.
fn open_log(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    rotate_if_large(path, LOG_ROTATE_BYTES)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Point this process's stderr at the log file, so every later
/// `eprintln!` and `tracing` event lands there.
pub fn redirect_stderr_to(path: &Path) -> io::Result<()> {
    let file = open_log(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: both are open descriptors owned by this process. `dup2`
        // makes fd 2 a second descriptor for the file, so the original can
        // be closed when `file` drops.
        if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::IntoRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
        // The standard handle keeps the file open for the life of the
        // process, so ownership passes to it rather than back to `file`.
        let handle = HANDLE(file.into_raw_handle());
        // SAFETY: `handle` is a valid, open file handle this process owns
        // and never closes. Rust's stderr looks the standard handle up on
        // every write, so later writes go to the file.
        unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) }.map_err(io::Error::other)?;
    }
    Ok(())
}

/// The startup wiring `main` calls: redirect stderr if [`should_redirect`]
/// says so, then start `tracing`. Returns the log path when redirected.
pub fn init(location: &ConfigLocation) -> Option<PathBuf> {
    use std::io::IsTerminal;
    let redirected = should_redirect(location, io::stderr().is_terminal()).then(|| {
        let path = log_file_path(location);
        match redirect_stderr_to(&path) {
            Ok(()) => Some(path),
            Err(e) => {
                eprintln!("uia: cannot write the log at {}: {e}", path.display());
                None
            }
        }
    });
    init_tracing();
    redirected.flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uia-logging-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_packaged_build_logs_beside_its_config() {
        let location = ConfigLocation::User(PathBuf::from("/home/me/uia-client"));
        assert_eq!(
            log_file_path(&location),
            PathBuf::from("/home/me/uia-client/logs/uia.log")
        );
    }

    #[test]
    fn only_a_packaged_build_without_a_terminal_redirects() {
        let user = ConfigLocation::User(PathBuf::from("/u"));
        let checkout = ConfigLocation::Development(PathBuf::from("/repo"));
        assert!(should_redirect(&user, false));
        assert!(!should_redirect(&user, true), "a terminal keeps its output");
        assert!(
            !should_redirect(&checkout, false),
            "a checkout never redirects"
        );
        assert!(!should_redirect(&checkout, true));
    }

    #[test]
    fn a_large_log_is_moved_aside_and_a_small_one_kept() {
        let dir = scratch("rotate");
        let log = dir.join(LOG_FILE);
        std::fs::write(&log, vec![b'x'; 11]).unwrap();
        rotate_if_large(&log, 10).unwrap();
        assert!(!log.exists());
        assert_eq!(std::fs::read(dir.join("uia.log.1")).unwrap().len(), 11);

        std::fs::write(&log, b"small").unwrap();
        rotate_if_large(&log, 10).unwrap();
        assert_eq!(std::fs::read(&log).unwrap(), b"small");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_log_is_not_an_error() {
        let dir = scratch("missing");
        rotate_if_large(&dir.join(LOG_FILE), 10).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_default_filter_shows_the_apps_info_and_others_warnings() {
        let targets = filter(None);
        assert!(targets.would_enable("uia_mcp::alert_listener", &tracing::Level::INFO));
        assert!(!targets.would_enable("uia_mcp::alert_listener", &tracing::Level::DEBUG));
        assert!(!targets.would_enable("rmcp::service", &tracing::Level::INFO));
        assert!(targets.would_enable("rmcp::service", &tracing::Level::WARN));
    }

    #[test]
    fn an_override_replaces_the_default_and_a_bad_one_is_ignored() {
        let debug = filter(Some("warn,uia_mcp=debug"));
        assert!(debug.would_enable("uia_mcp::alert_listener", &tracing::Level::DEBUG));
        let bad = filter(Some("uia_mcp=nonsense"));
        assert!(bad.would_enable("uia_mcp::alert_listener", &tracing::Level::INFO));
    }

    /// Set only in the child process `stderr_lands_in_the_log` starts.
    const CHILD_LOG_ENV: &str = "UIA_LOGGING_TEST_CHILD_LOG";

    /// The child half of `stderr_lands_in_the_log`: does nothing unless that
    /// test started it. Redirecting the test runner's own stderr would hide
    /// every later test's output, hence a separate process.
    #[test]
    fn redirect_child() {
        let Ok(path) = std::env::var(CHILD_LOG_ENV) else {
            return;
        };
        redirect_stderr_to(Path::new(&path)).unwrap();
        init_tracing();
        eprintln!("from eprintln");
        tracing::info!(target: "uia_mcp::alert_listener", "from tracing");
    }

    #[test]
    fn stderr_lands_in_the_log() {
        let dir = scratch("redirect");
        let log = dir.join(LOG_DIR_NAME).join(LOG_FILE);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "logging::tests::redirect_child", "--nocapture"])
            .env(CHILD_LOG_ENV, &log)
            .env_remove(LOG_FILTER_ENV)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let text = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(text.contains("from eprintln"), "{text}");
        assert!(text.contains("from tracing"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("private");
        let log = dir.join(LOG_DIR_NAME).join(LOG_FILE);
        drop(open_log(&log).unwrap());
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}

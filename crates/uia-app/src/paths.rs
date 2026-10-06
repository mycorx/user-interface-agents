// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Where config and produced files live.
//!
//! Two modes, and which one applies is decided by a single question: is there
//! a `uia.toml` in the current directory or above it? If yes, this is a
//! checkout and everything stays beside that file, exactly as it did before
//! this module existed. If no, this is a packaged install and everything goes
//! to one per-user directory.
//!
//! That rule is deliberately the same search `main` already performed to find
//! the config file, so development behaviour is unchanged by construction
//! rather than by a separate code path that has to be kept in sync.
//!
//! This module must not depend on `tauri`: `config` and `settings` are not
//! behind the `desktop` feature and are compiled (and tested) without it.

use std::path::{Path, PathBuf};

/// The single per-user directory a packaged build reads and writes.
pub const CONFIG_DIR_NAME: &str = "uia-client";

/// The config file, in both modes.
pub const CONFIG_FILE: &str = "uia.toml";

/// Overrides discovery entirely. Set it to run a packaged build against a
/// scratch directory, or to keep a portable install self-contained.
pub const CONFIG_DIR_ENV: &str = "UIA_CONFIG_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLocation {
    /// A `uia.toml` was found here or above; behave as before.
    Development(PathBuf),
    /// No checkout config; use the per-user directory.
    User(PathBuf),
}

impl ConfigLocation {
    pub fn config_dir(&self) -> &Path {
        match self {
            ConfigLocation::Development(d) | ConfigLocation::User(d) => d,
        }
    }

    /// The config file path. In `User` mode the file may not exist yet --
    /// every `Config` field defaults, so a missing `uia.toml` is not fatal,
    /// and `main` guards the read for exactly that case.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join(CONFIG_FILE)
    }
}

/// Pure: every input is injected so the decision can be tested without
/// touching the real environment or home directory.
///
/// Precedence, highest first:
///   1. `env_override` -- an explicit instruction, so it outranks discovery.
///   2. a `uia.toml` in `start` or an ancestor -- a checkout.
///   3. `user_dir` -- the packaged case.
///   4. `.` -- no home directory available. Matches where the pre-consolidation
///      bare-relative fallback wrote, so this is not a new behaviour.
pub fn resolve_from(
    start: &Path,
    env_override: Option<PathBuf>,
    user_dir: Option<PathBuf>,
) -> ConfigLocation {
    if let Some(forced) = env_override {
        return ConfigLocation::User(forced);
    }
    if let Some(found) = crate::config::find_config_file(start, CONFIG_FILE) {
        let dir = found
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        return ConfigLocation::Development(dir);
    }
    ConfigLocation::User(user_dir.unwrap_or_else(|| PathBuf::from(".")))
}

/// The platform's own local config location, suffixed `uia-client`:
///   Windows  %LOCALAPPDATA%\uia-client
///   macOS    ~/Library/Application Support/uia-client
///   Linux    $XDG_CONFIG_HOME/uia-client, else ~/.config/uia-client
///
/// `BaseDirs` rather than `ProjectDirs`, deliberately: `ProjectDirs` appends a
/// `config` subfolder on Windows and not on the other two, so it produces a
/// different shape per platform. Measured:
///
/// ```text
/// ProjectDirs::from("","","uia-client").config_local_dir()
///     = C:\Users\<u>\AppData\Local\uia-client\config
/// BaseDirs::new().config_local_dir()
///     = C:\Users\<u>\AppData\Local
/// ```
///
/// `BaseDirs` returns the bare root and this joins one known name onto it,
/// which is what makes the folder identical everywhere.
///
/// `config_local_dir` rather than `config_dir`: on Windows that is
/// %LOCALAPPDATA% instead of the roaming %APPDATA%. Roaming would carry
/// `uia-audio.json`'s machine-specific device names to machines that do not
/// have those devices, where `uia_audio`'s deliberate "unmatched device is an
/// error" path would refuse to start audio. On macOS and Linux the two are the
/// same directory.
///
/// `None` when there is no home directory to derive one from -- a service
/// account, or a stripped container. Callers fall back to the current
/// directory rather than refusing to start.
pub fn user_config_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_local_dir().join(CONFIG_DIR_NAME))
}

/// The one function here that reads global state. Everything it learns is
/// handed to `resolve_from`, which stays pure and is where the logic is tested.
pub fn resolve() -> ConfigLocation {
    let env_override = std::env::var_os(CONFIG_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_from(&start, env_override, user_config_dir())
}

/// Creates the directory in `User` mode. A no-op in `Development` mode: a
/// `uia.toml` was found inside that directory, so it exists by definition.
pub fn ensure_dir(loc: &ConfigLocation) -> std::io::Result<()> {
    match loc {
        ConfigLocation::Development(_) => Ok(()),
        ConfigLocation::User(dir) => std::fs::create_dir_all(dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkout with a uia.toml in it must keep behaving exactly as it does
    /// today: files live beside that file, wherever up the tree it was found.
    /// This is the case CI and every developer already depends on.
    #[test]
    fn a_uia_toml_in_an_ancestor_selects_development_mode() {
        let root = std::env::temp_dir().join(format!("uia-paths-dev-{}", std::process::id()));
        let nested = root.join("crates").join("uia-app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(CONFIG_FILE), "").unwrap();

        let found = resolve_from(&nested, None, Some(PathBuf::from("/unused/user/dir")));

        std::fs::remove_dir_all(&root).ok();
        assert_eq!(found, ConfigLocation::Development(root.clone()));
        assert_eq!(found.config_dir(), root.as_path());
        assert_eq!(found.config_file(), root.join(CONFIG_FILE));
    }

    /// The packaged case: nothing up the tree, so the per-user directory wins.
    #[test]
    fn no_ancestor_config_selects_the_user_directory() {
        let empty = std::env::temp_dir().join(format!("uia-paths-user-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let user = PathBuf::from("/home/someone/.config/uia-client");

        let found = resolve_from(&empty, None, Some(user.clone()));

        std::fs::remove_dir_all(&empty).ok();
        assert_eq!(found, ConfigLocation::User(user));
    }

    /// The override beats a real checkout config, so a test or a portable
    /// install can force a location without moving files around.
    #[test]
    fn the_env_override_outranks_an_ancestor_config() {
        let root = std::env::temp_dir().join(format!("uia-paths-ovr-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(CONFIG_FILE), "").unwrap();
        let forced = PathBuf::from("/forced/location");

        let found = resolve_from(
            &root,
            Some(forced.clone()),
            Some(PathBuf::from("/user/dir")),
        );

        std::fs::remove_dir_all(&root).ok();
        assert_eq!(found, ConfigLocation::User(forced));
    }

    /// No checkout config and no home directory to fall back on. Returning the
    /// current directory keeps the app usable rather than refusing to start;
    /// it is the same place today's bare `./uia-engine.json` fallback wrote to.
    #[test]
    fn an_unavailable_user_directory_falls_back_to_the_current_directory() {
        let empty = std::env::temp_dir().join(format!("uia-paths-nohome-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();

        let found = resolve_from(&empty, None, None);

        std::fs::remove_dir_all(&empty).ok();
        assert_eq!(found, ConfigLocation::User(PathBuf::from(".")));
    }

    /// Guards the BaseDirs choice. `ProjectDirs::config_local_dir()` looks like
    /// the right API and returns `...\Local\uia-client\config` on Windows --
    /// a nested subfolder that appears on Windows but not on Linux or macOS.
    /// Asserting the final component is exactly `uia-client` catches anyone
    /// switching back to it.
    #[test]
    fn the_user_directory_is_named_exactly_uia_client() {
        let Some(dir) = user_config_dir() else {
            // No home directory in this environment; nothing to assert.
            return;
        };
        assert_eq!(
            dir.file_name().and_then(|s| s.to_str()),
            Some(CONFIG_DIR_NAME),
            "got: {dir:?} -- a trailing component like `config` means \
             ProjectDirs crept back in; this must be BaseDirs + join"
        );
    }

    /// Windows-only, and the whole point of the change: local, never roaming.
    /// `uia-audio.json` stores device names, and a roamed profile naming a
    /// device this machine lacks hits `uia_audio`'s deliberate hard error.
    #[cfg(windows)]
    #[test]
    fn the_windows_user_directory_is_under_localappdata() {
        let (Some(dir), Ok(local)) = (user_config_dir(), std::env::var("LOCALAPPDATA")) else {
            return;
        };
        assert!(
            dir.starts_with(&local),
            "expected {dir:?} under LOCALAPPDATA ({local}); \
             Roaming here means config_dir was used instead of config_local_dir"
        );
    }

    /// `ensure_dir` must be usable on a path that does not exist yet -- that is
    /// the entire packaged first-run case.
    #[test]
    fn ensure_dir_creates_a_missing_user_directory() {
        let dir = std::env::temp_dir().join(format!("uia-paths-mk-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let loc = ConfigLocation::User(dir.clone());

        ensure_dir(&loc).unwrap();

        let existed = dir.is_dir();
        std::fs::remove_dir_all(&dir).ok();
        assert!(existed, "ensure_dir did not create {dir:?}");
    }

    /// The nine sidecars are siblings of the config file. That is what makes
    /// pointing `config_path` at the user directory sufficient -- no call site
    /// needs to know which mode is active. Asserted here against two of the
    /// real helpers so the property cannot quietly stop holding.
    #[test]
    fn sidecar_paths_follow_the_resolved_directory() {
        let dir = PathBuf::from("/some/resolved/dir");
        let loc = ConfigLocation::User(dir.clone());
        let cfg = loc.config_file();

        assert_eq!(
            crate::settings::settings_path_for(&cfg),
            dir.join("uia-engine.json")
        );
        assert_eq!(
            crate::personas::personas_path_for(&cfg),
            dir.join("uia-personas.json")
        );
    }

    /// End-to-end over the real write path, not just path arithmetic: resolve
    /// a User location the way a packaged build does, hand the derived path to
    /// the same `settings::save` the Settings UI calls, and assert the bytes
    /// land inside the resolved directory. `ensure_dir` first, because in User
    /// mode the directory does not exist until the app makes it.
    #[test]
    fn a_saved_sidecar_lands_inside_the_resolved_user_directory() {
        let dir = std::env::temp_dir().join(format!("uia-paths-e2e-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let empty = dir.join("launched-from-here");
        std::fs::create_dir_all(&empty).unwrap();

        let loc = resolve_from(&empty, None, Some(dir.clone()));
        assert_eq!(loc, ConfigLocation::User(dir.clone()));
        ensure_dir(&loc).unwrap();

        let sidecar = crate::settings::settings_path_for(&loc.config_file());
        crate::settings::save(
            &sidecar,
            &crate::settings::EngineSettings {
                engine: crate::config::EngineChoice::OpenAi,
            },
        )
        .unwrap();

        let landed = dir.join("uia-engine.json");
        let exists = landed.is_file();
        std::fs::remove_dir_all(&dir).ok();
        assert!(exists, "expected a written sidecar at {landed:?}");
    }
}

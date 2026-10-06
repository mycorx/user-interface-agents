// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Parsing `manifest.json` and resolving what it says to launch.
//!
//! Deliberately pure: every platform decision is an argument, so the
//! Windows override path is testable from Linux CI.

use super::BundleError;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The MCPB template variable standing for the extracted bundle's root.
const DIRNAME_TEMPLATE: &str = "${__dirname}";

/// Only the fields uia acts on. `manifest_version`/`dxt_version`,
/// `description`, `author`, `tools`, and the rest of the schema are ignored
/// rather than rejected: an unknown key must not make a valid bundle
/// uninstallable, and none of them affect what gets executed.
#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct McpbManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    pub server: McpbServer,
}

#[derive(Debug, Deserialize, PartialEq, Clone)]
pub struct McpbServer {
    /// `"binary"`, `"node"` or `"python"`. Only the first is installable;
    /// the check itself lives in `validate`, not here, so parsing stays
    /// free of policy.
    #[serde(rename = "type")]
    pub server_type: String,
    #[serde(default)]
    pub entry_point: Option<String>,
    #[serde(default)]
    pub mcp_config: Option<McpbMcpConfig>,
}

/// `BTreeMap` rather than `HashMap` so `env` resolves in a stable order and
/// tests can assert on a `Vec` without sorting.
#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
pub struct McpbMcpConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Keyed by MCPB platform name: `win32`, `darwin`, `linux`.
    #[serde(default)]
    pub platform_overrides: BTreeMap<String, McpbPlatformOverride>,
}

/// Every field optional and merged over the base: an override that names
/// only a Windows `.exe` must keep the base `args`, or the server launches
/// with no arguments on exactly one platform.
#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
pub struct McpbPlatformOverride {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
}

/// What the bundle says to run, with every template already substituted.
#[derive(Debug, PartialEq, Clone)]
pub struct ResolvedLaunch {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// MCPB platform names are Node's, not Rust's: `win32`/`darwin`/`linux`.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

pub fn parse_manifest(raw: &str) -> Result<McpbManifest, BundleError> {
    Ok(serde_json::from_str(raw)?)
}

fn substitute(value: &str, dirname: &str) -> String {
    value.replace(DIRNAME_TEMPLATE, dirname)
}

/// Merge the platform override over the base config, then substitute
/// `${__dirname}` everywhere.
///
/// `entry_point` is the fallback when no `mcp_config.command` is given: it
/// is bundle-relative by definition, so it is joined onto `dirname` rather
/// than substituted.
pub fn resolve_launch(
    manifest: &McpbManifest,
    platform: &str,
    dirname: &str,
) -> Result<ResolvedLaunch, BundleError> {
    let base = manifest.server.mcp_config.clone().unwrap_or_default();
    let over = base.platform_overrides.get(platform).cloned();

    let command = over
        .as_ref()
        .and_then(|o| o.command.clone())
        .or_else(|| base.command.clone())
        .map(|c| substitute(&c, dirname))
        .or_else(|| {
            manifest
                .server
                .entry_point
                .as_ref()
                .map(|e| format!("{dirname}/{}", substitute(e, dirname)))
        })
        .ok_or(BundleError::NoCommand)?;

    let args = over
        .as_ref()
        .and_then(|o| o.args.clone())
        .unwrap_or_else(|| base.args.clone())
        .iter()
        .map(|a| substitute(a, dirname))
        .collect();

    let env = over
        .as_ref()
        .and_then(|o| o.env.clone())
        .unwrap_or_else(|| base.env.clone())
        .iter()
        .map(|(k, v)| (k.clone(), substitute(v, dirname)))
        .collect();

    Ok(ResolvedLaunch { command, args, env })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape a real binary MCPB bundle ships. `${__dirname}` is the
    /// MCPB template variable for the extracted bundle root — the command
    /// is meaningless until it is substituted.
    const BINARY_MANIFEST: &str = r#"{
        "manifest_version": "0.2",
        "name": "clock",
        "version": "1.4.0",
        "server": {
            "type": "binary",
            "entry_point": "server/clock",
            "mcp_config": {
                "command": "${__dirname}/server/clock",
                "args": ["--stdio"],
                "env": { "CLOCK_TZ": "UTC" }
            }
        }
    }"#;

    #[test]
    fn a_binary_manifest_parses_into_its_declared_fields() {
        let m = parse_manifest(BINARY_MANIFEST).unwrap();
        assert_eq!(m.name, "clock");
        assert_eq!(m.version.as_deref(), Some("1.4.0"));
        assert_eq!(m.server.server_type, "binary");
        assert_eq!(m.server.entry_point.as_deref(), Some("server/clock"));
    }

    /// `dxt_version` is the older spelling of `manifest_version`. Both are
    /// ignored for parsing, but a manifest carrying only the old key must
    /// still parse — real bundles in the wild predate the rename.
    #[test]
    fn the_older_dxt_version_key_still_parses() {
        let raw = r#"{
            "dxt_version": "0.1",
            "name": "legacy",
            "version": "0.0.1",
            "server": { "type": "binary", "entry_point": "bin/legacy" }
        }"#;
        assert_eq!(parse_manifest(raw).unwrap().name, "legacy");
    }

    #[test]
    fn malformed_json_is_a_manifest_error_not_a_panic() {
        assert!(matches!(
            parse_manifest("{ not json"),
            Err(BundleError::Manifest(_))
        ));
    }

    /// The whole point of `${__dirname}`: an installed bundle lives at a
    /// path nobody knew when the manifest was written.
    #[test]
    fn dirname_is_substituted_through_command_args_and_env() {
        let m = parse_manifest(BINARY_MANIFEST).unwrap();
        let launch = resolve_launch(&m, "linux", "/local-servers/clock").unwrap();
        assert_eq!(launch.command, "/local-servers/clock/server/clock");
        assert_eq!(launch.args, vec!["--stdio".to_string()]);
        assert_eq!(
            launch.env,
            vec![("CLOCK_TZ".to_string(), "UTC".to_string())]
        );
    }

    /// A binary bundle ships one executable per platform, so the override
    /// table is how the Windows `.exe` gets picked on Windows and not
    /// anywhere else.
    #[test]
    fn a_platform_override_replaces_the_command_for_that_platform_only() {
        let raw = r#"{
            "manifest_version": "0.2",
            "name": "clock",
            "version": "1.0.0",
            "server": {
                "type": "binary",
                "entry_point": "server/clock",
                "mcp_config": {
                    "command": "${__dirname}/server/clock",
                    "args": ["--stdio"],
                    "platform_overrides": {
                        "win32": {
                            "command": "${__dirname}/server/clock.exe",
                            "args": ["--stdio", "--windows"]
                        }
                    }
                }
            }
        }"#;
        let m = parse_manifest(raw).unwrap();

        let win = resolve_launch(&m, "win32", "C:/local-servers/clock").unwrap();
        assert_eq!(win.command, "C:/local-servers/clock/server/clock.exe");
        assert_eq!(
            win.args,
            vec!["--stdio".to_string(), "--windows".to_string()]
        );

        // The override table must not leak onto other platforms.
        let mac = resolve_launch(&m, "darwin", "/local-servers/clock").unwrap();
        assert_eq!(mac.command, "/local-servers/clock/server/clock");
        assert_eq!(mac.args, vec!["--stdio".to_string()]);
    }

    /// An override that sets only `command` must keep the base `args`,
    /// rather than silently launching the server with no arguments.
    #[test]
    fn a_partial_override_inherits_the_base_args() {
        let raw = r#"{
            "manifest_version": "0.2",
            "name": "clock",
            "version": "1.0.0",
            "server": {
                "type": "binary",
                "entry_point": "server/clock",
                "mcp_config": {
                    "command": "${__dirname}/server/clock",
                    "args": ["--stdio"],
                    "platform_overrides": {
                        "win32": { "command": "${__dirname}/server/clock.exe" }
                    }
                }
            }
        }"#;
        let m = parse_manifest(raw).unwrap();
        let win = resolve_launch(&m, "win32", "C:/p").unwrap();
        assert_eq!(win.command, "C:/p/server/clock.exe");
        assert_eq!(win.args, vec!["--stdio".to_string()]);
    }

    /// `mcp_config` is optional in the schema; `entry_point` is then the
    /// only thing that says what to run, and it is bundle-relative.
    #[test]
    fn a_manifest_without_mcp_config_falls_back_to_the_entry_point() {
        let raw = r#"{
            "manifest_version": "0.2",
            "name": "bare",
            "version": "1.0.0",
            "server": { "type": "binary", "entry_point": "bin/bare" }
        }"#;
        let m = parse_manifest(raw).unwrap();
        let launch = resolve_launch(&m, "linux", "/local-servers/bare").unwrap();
        assert_eq!(launch.command, "/local-servers/bare/bin/bare");
        assert!(launch.args.is_empty());
    }

    /// Neither a command nor an entry point means there is nothing to run;
    /// failing here beats spawning an empty string later.
    #[test]
    fn a_manifest_with_nothing_to_run_is_rejected() {
        let raw = r#"{
            "manifest_version": "0.2",
            "name": "empty",
            "version": "1.0.0",
            "server": { "type": "binary" }
        }"#;
        let m = parse_manifest(raw).unwrap();
        assert!(matches!(
            resolve_launch(&m, "linux", "/p"),
            Err(BundleError::NoCommand)
        ));
    }

    #[test]
    fn current_platform_is_one_of_the_three_mcpb_names() {
        assert!(matches!(current_platform(), "win32" | "darwin" | "linux"));
    }
}

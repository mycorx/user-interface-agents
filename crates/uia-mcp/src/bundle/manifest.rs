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
    /// Settings the user fills in once; referenced as `${user_config.<key>}`.
    #[serde(default, deserialize_with = "lenient_user_config")]
    pub user_config: BTreeMap<String, UserConfigField>,
    /// Which operating systems the bundle was built for.
    #[serde(default, deserialize_with = "lenient_compatibility")]
    pub compatibility: Compatibility,
}

/// The slice of the manifest's `compatibility` block uia acts on.
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Compatibility {
    /// MCPB platform names (`win32`/`darwin`/`linux`). Empty means the bundle
    /// does not restrict itself.
    pub platforms: Vec<String>,
}

/// A `compatibility` that is not an object, or whose `platforms` is not an
/// array, is unrestricted; non-string items are dropped. Not worth rejecting a
/// bundle over.
fn lenient_compatibility<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Compatibility, D::Error> {
    let platforms = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(mut o) => match o.remove("platforms") {
            Some(serde_json::Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    Ok(Compatibility { platforms })
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

/// One entry of a manifest's `user_config`. Only what the Settings form and
/// the substitution need; unknown keys are ignored like everywhere else here.
///
/// Every attribute is parsed leniently (see the helpers below): a bundle author
/// writing `"required": "true"` or `"min": "0"` must not make the whole bundle
/// uninstallable, or turn an installed one Failed. The rest of the manifest
/// stays strict, because `name` and `server` decide what gets executed.
#[derive(Debug, Deserialize, PartialEq, Clone, Default)]
pub struct UserConfigField {
    /// `string`, `number`, `boolean`, `directory` or `file`. Anything else
    /// (including a wrongly typed value, which becomes `""`) is treated as
    /// `string` by its consumers.
    #[serde(rename = "type", default, deserialize_with = "lenient_string_or_empty")]
    pub kind: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "lenient_bool_false")]
    pub required: bool,
    /// Present but unparseable means `true`: showing or storing a would-be
    /// secret as plain text is worse than hiding an ordinary value.
    #[serde(default, deserialize_with = "lenient_bool_true")]
    pub sensitive: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient_number")]
    pub min: Option<f64>,
    #[serde(default, deserialize_with = "lenient_number")]
    pub max: Option<f64>,
    #[serde(default, deserialize_with = "lenient_bool_false")]
    pub multiple: bool,
}

/// A JSON bool, or the text `true`/`false` in any case; `None` otherwise.
fn coerce_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Some(true),
        serde_json::Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn lenient_bool_false<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(coerce_bool(&v).unwrap_or(false))
}

fn lenient_bool_true<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(coerce_bool(&v).unwrap_or(true))
}

fn lenient_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

fn lenient_string_or_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(lenient_string(d)?.unwrap_or_default())
}

fn lenient_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    })
}

/// A `user_config` that is not an object is treated as empty, and an entry
/// that is not an object (`null`, a string) is skipped: neither is worth
/// rejecting a bundle over.
fn lenient_user_config<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, UserConfigField>, D::Error> {
    let serde_json::Value::Object(entries) = serde_json::Value::deserialize(d)? else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .into_iter()
        .filter(|(_, v)| v.is_object())
        .filter_map(|(k, v)| serde_json::from_value(v).ok().map(|f| (k, f)))
        .collect())
}

impl UserConfigField {
    /// The manifest default as the text that would be substituted. A list
    /// default (`multiple`) yields its first element — joining several values
    /// is out of scope.
    pub fn default_text(&self) -> Option<String> {
        fn text(v: &serde_json::Value) -> Option<String> {
            match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Array(a) => a.first().and_then(text),
                _ => None,
            }
        }
        self.default.as_ref().and_then(text)
    }
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
#[derive(PartialEq, Clone)]
pub struct ResolvedLaunch {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Hand-written ON PURPOSE: after `apply_user_config` the args and env hold
/// substituted setting values, which can be keyring secrets. Prints the
/// command, the env variable names and the argument count only.
impl std::fmt::Debug for ResolvedLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedLaunch")
            .field("command", &self.command)
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .field(
                "env",
                &self.env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            )
            .finish()
    }
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

const USER_CONFIG_OPEN: &str = "${user_config.";

/// Replace `${NAME}` with `vars[NAME]` for every NAME that is a key of `vars`.
///
/// This is MCPB's path-variable pass (`${HOME}`, `${DOCUMENTS}`, `${/}`, ...).
/// It only ever runs on text the manifest author wrote. Anything not in `vars`
/// (`${user_config.x}`, `${__dirname}`, a typo) and an unterminated `${` are
/// left as they are, so an unresolvable directory never fails a launch.
/// Single pass: a replacement is never rescanned.
pub fn expand_path_vars(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after
            .find('}')
            .and_then(|end| vars.get(&after[..end]).map(|v| (end, v)))
        {
            Some((end, value)) => {
                out.push_str(value);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Substitute `${user_config.<key>}` in an already-validated launch.
///
/// Runs after `validate_bundle` on purpose: the trust checks must judge the
/// manifest as shipped, not as a user configured it. Path variables are
/// expanded FIRST, in args and env values only (never the command, which must
/// stay inside the bundle, and never env keys); the user-config pass runs
/// second, so a typed value or a keyring secret is inserted after path
/// expansion and can never be treated as a path template. Each pass is single
/// (its own replacement text is not rescanned), but the path pass's output IS
/// scanned by the user-config pass; user values are never expanded, so one
/// that happens to contain `${...}` stays data.
pub fn apply_user_config(
    launch: ResolvedLaunch,
    fields: &BTreeMap<String, UserConfigField>,
    values: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
) -> Result<ResolvedLaunch, BundleError> {
    let sub =
        |text: &str| substitute_user_config(&expand_path_vars(text, vars), fields, values, vars);
    Ok(ResolvedLaunch {
        command: substitute_user_config(&launch.command, fields, values, vars)?,
        args: launch
            .args
            .iter()
            .map(|a| sub(a))
            .collect::<Result<_, _>>()?,
        env: launch
            .env
            .iter()
            .map(|(k, v)| Ok((k.clone(), sub(v)?)))
            .collect::<Result<_, BundleError>>()?,
    })
}

fn substitute_user_config(
    text: &str,
    fields: &BTreeMap<String, UserConfigField>,
    values: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
) -> Result<String, BundleError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(USER_CONFIG_OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + USER_CONFIG_OPEN.len()..];
        let Some(end) = after.find('}') else {
            // Unterminated: never launch with literal `${user_config.*}` text.
            // Reported as an undeclared key, since the key has no end.
            return Err(BundleError::UndeclaredUserConfig(after.to_string()));
        };
        out.push_str(&resolve_user_config(&after[..end], fields, values, vars)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve_user_config(
    key: &str,
    fields: &BTreeMap<String, UserConfigField>,
    values: &BTreeMap<String, String>,
    vars: &BTreeMap<String, String>,
) -> Result<String, BundleError> {
    let field = fields
        .get(key)
        .ok_or_else(|| BundleError::UndeclaredUserConfig(key.to_string()))?;
    if let Some(v) = values.get(key).filter(|v| !v.is_empty()) {
        return Ok(v.clone());
    }
    if let Some(d) = field.default_text() {
        // Manifest-authored, so path variables expand here; the typed value
        // returned above never does.
        return Ok(expand_path_vars(&d, vars));
    }
    if field.required {
        Err(BundleError::MissingUserConfig(key.to_string()))
    } else {
        Ok(String::new())
    }
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

    /// Modeled on the `mymy-assistant` bundle's manifest, with extra fields
    /// (`api_token`, `retries`) added to exercise more setting types.
    const USER_CONFIG_MANIFEST: &str = r#"{
        "manifest_version": "0.3",
        "name": "mymy-assistant",
        "version": "0.1.1",
        "server": {
            "type": "binary",
            "entry_point": "server/mymy-assistant",
            "mcp_config": {
                "command": "${__dirname}/server/mymy-assistant",
                "args": [],
                "env": { "ENABLE_SYSTEM_TOASTS": "${user_config.enable_system_toasts}" }
            }
        },
        "user_config": {
            "enable_system_toasts": {
                "type": "string",
                "title": "Desktop toast notifications",
                "description": "Set to exactly \"true\" to enable.",
                "default": "true",
                "required": false
            },
            "api_token": { "type": "string", "title": "Token", "sensitive": true, "required": true },
            "retries": { "type": "number", "min": 0, "max": 5, "default": 2, "future_key": 1 }
        }
    }"#;

    /// Substituted values can be keyring secrets, in env or in args.
    #[test]
    fn a_resolved_launchs_debug_prints_neither_env_values_nor_args() {
        let launch = ResolvedLaunch {
            command: "/b/server".into(),
            args: vec!["--token=sk-secret-arg".into()],
            env: vec![("API_TOKEN".into(), "sk-secret-env".into())],
        };
        let printed = format!("{launch:?}");
        assert!(printed.contains("/b/server"), "{printed}");
        assert!(printed.contains("API_TOKEN"), "{printed}");
        assert!(!printed.contains("sk-secret-env"), "{printed}");
        assert!(!printed.contains("sk-secret-arg"), "{printed}");
    }

    #[test]
    fn user_config_fields_parse_with_their_declared_attributes() {
        let m = parse_manifest(USER_CONFIG_MANIFEST).unwrap();
        let toasts = &m.user_config["enable_system_toasts"];
        assert_eq!(toasts.kind, "string");
        assert_eq!(toasts.title.as_deref(), Some("Desktop toast notifications"));
        assert!(!toasts.required && !toasts.sensitive);
        assert_eq!(toasts.default_text().as_deref(), Some("true"));
        assert!(m.user_config["api_token"].sensitive);
        assert!(m.user_config["api_token"].required);
        let retries = &m.user_config["retries"];
        assert_eq!((retries.min, retries.max), (Some(0.0), Some(5.0)));
        assert_eq!(retries.default_text().as_deref(), Some("2"));
    }

    #[test]
    fn a_manifest_without_user_config_has_an_empty_map() {
        assert!(
            parse_manifest(BINARY_MANIFEST)
                .unwrap()
                .user_config
                .is_empty()
        );
    }

    fn manifest_with_user_config(user_config: &str) -> String {
        format!(r#"{{"name":"m","server":{{"type":"binary"}},"user_config":{user_config}}}"#)
    }

    #[test]
    fn wrongly_typed_user_config_attributes_do_not_reject_the_manifest() {
        let m = parse_manifest(&manifest_with_user_config(
            r#"{
                "a": {"type":1,"title":5,"required":"true","min":"0","max":"5","multiple":"False"},
                "b": null,
                "c": "oops",
                "d": {"sensitive":"yes please"},
                "e": {"sensitive":"false"},
                "f": {"type":"number","min":2.5}
            }"#,
        ))
        .unwrap();
        let a = &m.user_config["a"];
        assert_eq!(a.kind, "");
        assert_eq!(a.title, None);
        assert!(a.required);
        assert_eq!((a.min, a.max), (Some(0.0), Some(5.0)));
        assert!(!a.multiple);
        assert!(!m.user_config.contains_key("b"));
        assert!(!m.user_config.contains_key("c"));
        // Fail safe: an unparseable `sensitive` must never become plain text.
        assert!(m.user_config["d"].sensitive);
        assert!(!m.user_config["e"].sensitive);
        let f = &m.user_config["f"];
        assert_eq!((f.kind.as_str(), f.min), ("number", Some(2.5)));
        assert!(!f.sensitive);
    }

    #[test]
    fn non_finite_min_max_strings_are_absent_and_nulls_are_handled() {
        for bad in ["NaN", "inf", "infinity", "-inf", "1e999"] {
            let m = parse_manifest(&manifest_with_user_config(&format!(
                r#"{{"a":{{"min":"{bad}","max":"{bad}"}}}}"#
            )))
            .unwrap();
            let a = &m.user_config["a"];
            assert_eq!((a.min, a.max), (None, None), "{bad}");
        }
        let m = parse_manifest(&manifest_with_user_config(
            r#"{"a":{"min":"2.5"},"b":{"sensitive":null,"min":null}}"#,
        ))
        .unwrap();
        assert_eq!(m.user_config["a"].min, Some(2.5));
        // Fail safe: a null `sensitive` is sensitive.
        assert!(m.user_config["b"].sensitive);
        assert_eq!(m.user_config["b"].min, None);
    }

    #[test]
    fn a_user_config_that_is_not_an_object_is_empty() {
        for raw in ["[]", "null", r#""x""#, "5"] {
            let m = parse_manifest(&manifest_with_user_config(raw)).unwrap();
            assert!(m.user_config.is_empty(), "{raw}");
        }
    }

    #[test]
    fn leniency_does_not_leak_to_the_rest_of_the_manifest() {
        let raw = r#"{"name":"m","server":{"type":5},"user_config":{}}"#;
        assert!(parse_manifest(raw).is_err());
        let raw = r#"{"name":"m","server":"binary"}"#;
        assert!(parse_manifest(raw).is_err());
    }

    #[test]
    fn default_text_renders_booleans_and_takes_the_first_of_a_list() {
        let f: UserConfigField =
            serde_json::from_str(r#"{"type":"boolean","default":false}"#).unwrap();
        assert_eq!(f.default_text().as_deref(), Some("false"));
        let f: UserConfigField =
            serde_json::from_str(r#"{"type":"directory","multiple":true,"default":["/a","/b"]}"#)
                .unwrap();
        assert_eq!(f.default_text().as_deref(), Some("/a"));
    }

    fn launch_with_env(value: &str) -> ResolvedLaunch {
        ResolvedLaunch {
            command: "/b/server".into(),
            args: vec!["--mode=${user_config.mode}".into()],
            env: vec![("TOASTS".into(), value.into())],
        }
    }

    fn fields() -> BTreeMap<String, UserConfigField> {
        parse_manifest(USER_CONFIG_MANIFEST).unwrap().user_config
    }

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_stored_value_wins_over_the_manifest_default() {
        let out = apply_user_config(
            launch_with_env("${user_config.enable_system_toasts}"),
            &fields_with_mode(),
            &vals(&[
                ("enable_system_toasts", "false"),
                ("mode", "x"),
                ("api_token", "t"),
            ]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env, vec![("TOASTS".to_string(), "false".to_string())]);
        assert_eq!(out.args, vec!["--mode=x".to_string()]);
    }

    #[test]
    fn the_manifest_default_applies_when_nothing_is_stored() {
        let out = apply_user_config(
            launch_with_env("${user_config.enable_system_toasts}"),
            &fields_with_mode(),
            &vals(&[("mode", "x"), ("api_token", "t")]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "true");
    }

    #[test]
    fn an_optional_field_with_no_value_and_no_default_is_empty() {
        let out = apply_user_config(
            launch_with_env("${user_config.mode}"),
            &fields_with_mode(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "");
    }

    #[test]
    fn a_required_field_with_no_value_and_no_default_is_an_error() {
        let err = apply_user_config(
            launch_with_env("${user_config.api_token}"),
            &fields_with_mode(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, BundleError::MissingUserConfig(k) if k == "api_token"));
    }

    #[test]
    fn a_reference_to_an_undeclared_key_is_an_error_not_a_literal() {
        let err = apply_user_config(
            launch_with_env("${user_config.typo}"),
            &fields_with_mode(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, BundleError::UndeclaredUserConfig(k) if k == "typo"));
    }

    /// A user-typed value must stay data: no second round of substitution.
    #[test]
    fn a_value_that_looks_like_a_template_is_not_expanded_again() {
        let out = apply_user_config(
            launch_with_env("${user_config.mode}"),
            &fields_with_mode(),
            &vals(&[("mode", "${__dirname}/${user_config.api_token}")]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "${__dirname}/${user_config.api_token}");
    }

    #[test]
    fn awkward_characters_reach_the_env_byte_for_byte() {
        let nasty = "a b \"q\" $HOME 'z' é 日本";
        let out = apply_user_config(
            launch_with_env("${user_config.mode}"),
            &fields_with_mode(),
            &vals(&[("mode", nasty)]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env[0].1, nasty);
    }

    #[test]
    fn text_around_a_reference_and_several_references_all_substitute() {
        let out = apply_user_config(
            launch_with_env("x-${user_config.mode}-${user_config.mode}-y"),
            &fields_with_mode(),
            &vals(&[("mode", "m")]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "x-m-m-y");
    }

    fn fields_with_mode() -> BTreeMap<String, UserConfigField> {
        let mut f = fields();
        f.insert(
            "mode".into(),
            UserConfigField {
                kind: "string".into(),
                ..Default::default()
            },
        );
        f
    }

    #[test]
    fn an_unterminated_reference_is_an_error_not_literal_text() {
        for text in ["a-${user_config.mode", "${user_config."] {
            let err = apply_user_config(
                launch_with_env(text),
                &fields_with_mode(),
                &vals(&[("mode", "m")]),
                &BTreeMap::new(),
            )
            .unwrap_err();
            assert!(
                matches!(err, BundleError::UndeclaredUserConfig(_)),
                "got {err:?}"
            );
        }
    }

    #[test]
    fn an_empty_key_is_an_undeclared_reference() {
        let err = apply_user_config(
            launch_with_env("${user_config.}"),
            &fields_with_mode(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, BundleError::UndeclaredUserConfig(k) if k.is_empty()));
    }

    #[test]
    fn multibyte_text_around_a_reference_survives() {
        let out = apply_user_config(
            launch_with_env("é-${user_config.mode}-日本"),
            &fields_with_mode(),
            &vals(&[("mode", "m")]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "é-m-日本");
    }

    #[test]
    fn expand_path_vars_replaces_a_known_variable() {
        let v = vals(&[("HOME", "/h")]);
        assert_eq!(expand_path_vars("${HOME}/x", &v), "/h/x");
    }

    #[test]
    fn expand_path_vars_leaves_unknown_and_other_templates_alone() {
        let v = vals(&[("HOME", "/h")]);
        for text in ["${FOO}", "${user_config.x}", "${__dirname}/a"] {
            assert_eq!(expand_path_vars(text, &v), text);
        }
    }

    #[test]
    fn expand_path_vars_handles_several_variables_and_surrounding_text() {
        let v = vals(&[("HOME", "/h"), ("/", "|")]);
        assert_eq!(expand_path_vars("a${HOME}b${/}c${HOME}", &v), "a/hb|c/h");
    }

    #[test]
    fn expand_path_vars_leaves_an_unterminated_reference() {
        let v = vals(&[("HOME", "/h")]);
        assert_eq!(expand_path_vars("x${HOME", &v), "x${HOME");
    }

    #[test]
    fn expand_path_vars_survives_multibyte_text() {
        let v = vals(&[("HOME", "/h")]);
        assert_eq!(expand_path_vars("é-${HOME}-日本", &v), "é-/h-日本");
        assert_eq!(expand_path_vars("日${本", &v), "日${本");
    }

    #[test]
    fn expand_path_vars_does_not_rescan_a_replacement() {
        let v = vals(&[("HOME", "${DESKTOP}"), ("DESKTOP", "/d")]);
        assert_eq!(expand_path_vars("${HOME}", &v), "${DESKTOP}");
    }

    #[test]
    fn a_manifest_default_has_path_variables_expanded() {
        let mut f = fields_with_mode();
        f.get_mut("mode").unwrap().default = Some(serde_json::json!("${HOME}/Desktop"));
        let out = apply_user_config(
            launch_with_env("${user_config.mode}"),
            &f,
            &BTreeMap::new(),
            &vals(&[("HOME", "/h")]),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "/h/Desktop");
    }

    #[test]
    fn args_and_env_values_expand_path_variables() {
        let launch = ResolvedLaunch {
            command: "/b/server".into(),
            args: vec!["--root=${DOCUMENTS}".into()],
            env: vec![("DIR".into(), "${DOWNLOADS}${/}x".into())],
        };
        let out = apply_user_config(
            launch,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &vals(&[("DOCUMENTS", "/docs"), ("DOWNLOADS", "/dl"), ("/", "/")]),
        )
        .unwrap();
        assert_eq!(out.args, vec!["--root=/docs".to_string()]);
        assert_eq!(out.env[0].1, "/dl/x");
    }

    /// The security point: a typed value is data, never a path template.
    #[test]
    fn a_user_typed_path_variable_stays_literal() {
        let out = apply_user_config(
            launch_with_env("${user_config.mode}"),
            &fields_with_mode(),
            &vals(&[("mode", "${HOME}")]),
            &vals(&[("HOME", "/h")]),
        )
        .unwrap();
        assert_eq!(out.env[0].1, "${HOME}");
    }

    /// One string mixing all three: only the manifest-authored `${HOME}`
    /// expands; the typed `${HOME}` stays data.
    #[test]
    fn combined_text_expands_only_the_manifest_authored_path_variable() {
        let launch = ResolvedLaunch {
            command: "/b/server".into(),
            args: vec!["--root=${HOME}/${user_config.mode}".into()],
            env: vec![("R".into(), "${HOME}/${user_config.mode}".into())],
        };
        let out = apply_user_config(
            launch,
            &fields_with_mode(),
            &vals(&[("mode", "${HOME}")]),
            &vals(&[("HOME", "/h")]),
        )
        .unwrap();
        assert_eq!(out.args, vec!["--root=/h/${HOME}".to_string()]);
        assert_eq!(out.env[0].1, "/h/${HOME}");
    }

    #[test]
    fn command_and_env_keys_are_never_path_expanded() {
        let launch = ResolvedLaunch {
            command: "${HOME}/server".into(),
            args: vec![],
            env: vec![("${HOME}".into(), "v".into())],
        };
        let out = apply_user_config(
            launch,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &vals(&[("HOME", "/h")]),
        )
        .unwrap();
        assert_eq!(out.command, "${HOME}/server");
        assert_eq!(out.env[0].0, "${HOME}");
    }

    fn platforms_of(compat: &str) -> Vec<String> {
        parse_manifest(&format!(
            r#"{{"name":"x","server":{{"type":"binary"}}{compat}}}"#
        ))
        .unwrap()
        .compatibility
        .platforms
    }

    #[test]
    fn declared_platforms_are_parsed() {
        assert_eq!(
            platforms_of(r#","compatibility":{"platforms":["darwin","win32"]}"#),
            vec!["darwin".to_string(), "win32".to_string()]
        );
    }

    #[test]
    fn a_malformed_compatibility_is_unrestricted_and_still_parses() {
        for compat in [
            "",
            r#","compatibility":5"#,
            r#","compatibility":null"#,
            r#","compatibility":"darwin""#,
            r#","compatibility":{"platforms":"darwin"}"#,
            r#","compatibility":{"platforms":[1,true]}"#,
            r#","compatibility":{"platforms":[]}"#,
        ] {
            assert!(platforms_of(compat).is_empty(), "{compat}");
        }
    }

    #[test]
    fn non_string_platform_entries_are_dropped_and_strings_kept() {
        assert_eq!(
            platforms_of(r#","compatibility":{"platforms":[1,"linux",null]}"#),
            vec!["linux".to_string()]
        );
    }
}

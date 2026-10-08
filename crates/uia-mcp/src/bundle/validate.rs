// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Whether uia is willing to run what a bundle declares.
//!
//! Three checks, deliberately overlapping. The declared `server.type` is
//! the bundle's own claim; the extension is evidence against that claim;
//! the leading bytes catch an extensionless script the extension list
//! cannot see. A fourth check — that the command is a file the bundle
//! actually shipped — is what stops a manifest from pointing at an
//! interpreter already on the machine and calling itself a binary local server.

use super::{BundleError, McpbManifest, ResolvedLaunch, resolve_launch};
use std::path::{Component, Path, PathBuf};

/// Extensions that mean "an interpreter runs this file". Not exhaustive
/// against a determined author — that is what the bundle-containment and
/// shebang checks are for — but it catches every ordinary case and gives a
/// clear reason instead of a vague refusal.
pub const SCRIPT_EXTENSIONS: &[&str] = &[
    "py", "js", "mjs", "cjs", "ts", "rb", "sh", "bash", "ps1", "bat", "cmd", "php", "pl",
];

/// Resolve `path` against nothing — just fold away `.` and `..` textually.
///
/// Deliberately NOT `canonicalize`: that requires the file to exist (so a
/// missing command would report the wrong error) and resolves symlinks
/// against the real filesystem, which makes the outcome depend on how the
/// zip happened to extract. A textual fold answers the only question being
/// asked — does this path, as written, stay inside the bundle.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The full trust decision for one bundle, returning what to launch.
///
/// `bundle_dir` is where the bundle's files actually live; `platform` is an
/// MCPB platform name (`win32`/`darwin`/`linux`), injected rather than
/// detected so the Windows path is testable from Linux CI.
pub fn validate_bundle(
    manifest: &McpbManifest,
    platform: &str,
    bundle_dir: &Path,
) -> Result<ResolvedLaunch, BundleError> {
    crate::validate_server_name(&manifest.name).map_err(|e| BundleError::Name(e.to_string()))?;

    if manifest.server.server_type != "binary" {
        return Err(BundleError::NotBinary(manifest.server.server_type.clone()));
    }

    let declared = &manifest.compatibility.platforms;
    if !declared.is_empty()
        && !declared
            .iter()
            .any(|p| p.trim().eq_ignore_ascii_case(platform))
    {
        return Err(BundleError::UnsupportedPlatform {
            declared: declared.clone(),
            current: platform.to_string(),
        });
    }

    let dirname = bundle_dir.to_string_lossy().replace('\\', "/");
    let launch = resolve_launch(manifest, platform, &dirname)?;

    let command = PathBuf::from(&launch.command);
    let resolved = normalize(&command);
    if !resolved.starts_with(normalize(bundle_dir)) {
        return Err(BundleError::EscapesBundle(launch.command.clone()));
    }
    // `user_config` is applied after validation, so a template left in the
    // command would let a setting pick a different executable than the one
    // approved here.
    if launch.command.contains("${user_config.") {
        return Err(BundleError::UserConfigInCommand(launch.command.clone()));
    }

    if let Some(ext) = resolved.extension().and_then(|e| e.to_str()) {
        let lower = ext.to_ascii_lowercase();
        if SCRIPT_EXTENSIONS.contains(&lower.as_str()) {
            return Err(BundleError::ScriptCommand(launch.command.clone(), lower));
        }
    }

    if !resolved.is_file() {
        return Err(BundleError::MissingCommand(launch.command.clone()));
    }

    // Two bytes is all a shebang is; reading the whole file would mean
    // loading an entire executable to look at its head.
    let mut head = [0u8; 2];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&resolved)?;
        // A file shorter than two bytes cannot carry `#!`; a short read is
        // not an error here, just evidence of that.
        let _ = f.read(&mut head)?;
    }
    if &head == b"#!" {
        return Err(BundleError::ShebangCommand(launch.command.clone()));
    }

    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::manifest::parse_manifest;

    /// A throwaway bundle directory holding one "executable" whose bytes we
    /// control — the shebang check reads them, so they matter.
    ///
    /// Several tests reuse the same `file` name (e.g. `"server/probe"`), and
    /// cargo runs tests in parallel by default, so the directory name is
    /// salted with a monotonic counter on top of the pid — without it,
    /// concurrent tests race on the same path (one test's cleanup deletes
    /// another's still-in-use fixture).
    fn bundle_with(file: &str, contents: &[u8]) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "uia-mcpb-{}-{n}-{}",
            std::process::id(),
            file.replace(['/', '.', '\\'], "_")
        ));
        std::fs::remove_dir_all(&dir).ok();
        let target = dir.join(file);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, contents).unwrap();
        dir
    }

    fn manifest_for(server_type: &str, command: &str) -> crate::bundle::McpbManifest {
        parse_manifest(&format!(
            r#"{{
                "manifest_version": "0.2",
                "name": "probe",
                "version": "1.0.0",
                "server": {{
                    "type": "{server_type}",
                    "entry_point": "server/probe",
                    "mcp_config": {{ "command": "{command}" }}
                }}
            }}"#
        ))
        .unwrap()
    }

    /// ELF magic: real compiled bytes, so nothing about this file reads as
    /// a script.
    const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00rest-of-a-binary";

    #[test]
    fn a_binary_bundle_shipping_a_real_executable_is_accepted() {
        let dir = bundle_with("server/probe", ELF);
        let m = manifest_for("binary", "${__dirname}/server/probe");

        let launch = validate_bundle(&m, "linux", &dir).unwrap();

        let command = launch.command.clone();
        std::fs::remove_dir_all(&dir).ok();
        assert!(command.ends_with("server/probe"), "got {command}");
    }

    /// The headline rule. A node/python bundle ships an interpreter and
    /// source, which is the arbitrary-code case requiring a bundle is
    /// meant to prevent.
    #[test]
    fn node_and_python_bundles_are_refused_by_declared_type() {
        for kind in ["node", "python"] {
            let dir = bundle_with("server/probe", ELF);
            let m = manifest_for(kind, "${__dirname}/server/probe");

            let err = validate_bundle(&m, "linux", &dir).unwrap_err();

            std::fs::remove_dir_all(&dir).ok();
            assert!(
                matches!(&err, BundleError::NotBinary(t) if t == kind),
                "{kind} produced {err:?}"
            );
        }
    }

    /// A bundle may claim `"binary"` and still point at a script. The
    /// declared type is the bundle's own word for it; the extension is
    /// evidence.
    #[test]
    fn a_binary_type_pointing_at_a_script_is_still_refused() {
        for ext in SCRIPT_EXTENSIONS {
            let file = format!("server/probe.{ext}");
            let dir = bundle_with(&file, b"print('hi')");
            let m = manifest_for("binary", &format!("${{__dirname}}/{file}"));

            let err = validate_bundle(&m, "linux", &dir).unwrap_err();

            std::fs::remove_dir_all(&dir).ok();
            assert!(
                matches!(&err, BundleError::ScriptCommand(_, e) if e == ext),
                ".{ext} produced {err:?}"
            );
        }
    }

    /// The extension list cannot catch an extensionless script, so the
    /// bytes get the last word.
    #[test]
    fn an_extensionless_file_with_a_shebang_is_refused() {
        let dir = bundle_with("server/probe", b"#!/usr/bin/env python3\nprint('hi')\n");
        let m = manifest_for("binary", "${__dirname}/server/probe");

        let err = validate_bundle(&m, "linux", &dir).unwrap_err();

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, BundleError::ShebangCommand(_)), "got {err:?}");
    }

    /// A manifest that walks out of its own directory would let a bundle
    /// run any file on the machine while still looking like a local server.
    #[test]
    fn a_command_escaping_the_bundle_directory_is_refused() {
        let dir = bundle_with("server/probe", ELF);
        let m = manifest_for("binary", "${__dirname}/../../../usr/bin/python3");

        let err = validate_bundle(&m, "linux", &dir).unwrap_err();

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, BundleError::EscapesBundle(_)), "got {err:?}");
    }

    /// A bare interpreter name is the same escape by another route: it is
    /// not a path into the bundle at all, so PATH would resolve it.
    #[test]
    fn a_bare_interpreter_name_is_refused_as_an_escape() {
        let dir = bundle_with("server/probe", ELF);
        let m = manifest_for("binary", "node");

        let err = validate_bundle(&m, "linux", &dir).unwrap_err();

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, BundleError::EscapesBundle(_)), "got {err:?}");
    }

    /// A manifest naming a file the zip never contained must fail at
    /// install rather than at the first session that tries to spawn it.
    #[test]
    fn a_command_that_is_not_in_the_bundle_is_refused() {
        let dir = bundle_with("server/probe", ELF);
        let m = manifest_for("binary", "${__dirname}/server/missing");

        let err = validate_bundle(&m, "linux", &dir).unwrap_err();

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, BundleError::MissingCommand(_)), "got {err:?}");
    }

    /// The local server name prefixes every one of its tool names, and OpenAI
    /// rejects the whole session.update over one bad character — which
    /// disables every tool in the session, not just this one's.
    #[test]
    fn a_local_server_name_that_would_break_the_tool_declaration_is_refused() {
        let dir = bundle_with("server/probe", ELF);
        let mut m = manifest_for("binary", "${__dirname}/server/probe");
        m.name = "my.clock".into();

        let err = validate_bundle(&m, "linux", &dir).unwrap_err();

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, BundleError::Name(_)), "got {err:?}");
    }

    /// `user_config` is applied AFTER validation, so a command that is only a
    /// template can never be validated into existence: it resolves outside
    /// the bundle. A user-set value must never get to choose the executable.
    #[test]
    fn a_command_that_is_only_a_user_config_reference_is_refused() {
        let dir = bundle_with("server/probe", b"\x7fELF");
        let m = manifest_for("binary", "${user_config.exe}");
        let err = validate_bundle(&m, "linux", &dir).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, BundleError::EscapesBundle(_)), "got {err:?}");
    }

    /// A template inside an otherwise in-bundle path would let a setting
    /// choose the executable after validation approved a different one.
    #[test]
    fn a_command_that_contains_a_user_config_reference_is_refused() {
        let dir = bundle_with("server/${user_config.x}", b"\x7fELF");
        let m = manifest_for("binary", "${__dirname}/server/${user_config.x}");
        let err = validate_bundle(&m, "linux", &dir).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(err, BundleError::UserConfigInCommand(_)),
            "got {err:?}"
        );
    }

    fn declaring(platforms: &[&str]) -> crate::bundle::McpbManifest {
        let mut m = manifest_for("binary", "${__dirname}/server/probe");
        m.compatibility.platforms = platforms.iter().map(|p| p.to_string()).collect();
        m
    }

    #[test]
    fn a_bundle_for_another_platform_is_refused_naming_both() {
        let dir = bundle_with("server/probe", ELF);
        let err = validate_bundle(&declaring(&["darwin"]), "linux", &dir).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(&err, BundleError::UnsupportedPlatform { declared, current }
                if declared == &["darwin"] && current == "linux"),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("darwin") && msg.contains("linux"), "{msg}");
    }

    #[test]
    fn a_bundle_for_this_platform_passes_case_insensitively() {
        let dir = bundle_with("server/probe", ELF);
        let exact = validate_bundle(&declaring(&["darwin"]), "darwin", &dir);
        let cased = validate_bundle(&declaring(&["Darwin"]), "darwin", &dir);
        let several = validate_bundle(&declaring(&["win32", "darwin"]), "darwin", &dir);
        let padded = validate_bundle(&declaring(&[" darwin\t"]), "darwin", &dir);
        std::fs::remove_dir_all(&dir).ok();
        assert!(exact.is_ok() && cased.is_ok() && several.is_ok() && padded.is_ok());
    }

    #[test]
    fn a_bundle_declaring_no_platform_runs_everywhere() {
        let dir = bundle_with("server/probe", ELF);
        let m = manifest_for("binary", "${__dirname}/server/probe");
        let results: Vec<_> = ["win32", "darwin", "linux"]
            .iter()
            .map(|p| validate_bundle(&m, p, &dir).is_ok())
            .collect();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(results, [true, true, true]);
    }

    #[test]
    fn a_windows_bundle_is_refused_on_macos() {
        let dir = bundle_with("server/probe", ELF);
        let err = validate_bundle(&declaring(&["win32"]), "darwin", &dir).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(err, BundleError::UnsupportedPlatform { .. }),
            "got {err:?}"
        );
    }
}

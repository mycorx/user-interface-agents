// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Extracting a `.mcpb` and committing it, only if it validates.
//!
//! Extraction goes to a staging directory beside the destination, not to
//! the destination itself: `validate_bundle` needs real files on disk to
//! read, and a bundle that fails must not leave a half-installed local server
//! sitting among approved ones. The rename at the end is the commit.

use super::{
    BundleError, McpbManifest, ResolvedLaunch, current_platform, parse_manifest, validate_bundle,
};
use std::path::{Component, Path, PathBuf};

/// What a successful install produced: the approved local server and exactly how
/// to launch it, so no caller has to re-read the manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledBundle {
    pub name: String,
    pub version: Option<String>,
    pub dir: PathBuf,
    pub launch: ResolvedLaunch,
}

/// Rejects any entry that would write outside `root`: absolute paths,
/// Windows drive prefixes, and any `..` component. `zip`'s own
/// `enclosed_name` does most of this, but returning `None` there is
/// indistinguishable from a merely unusual name, so the check is explicit.
fn safe_entry_path(root: &Path, raw: &str) -> Result<PathBuf, BundleError> {
    let candidate = Path::new(raw);
    for part in candidate.components() {
        match part {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(BundleError::Archive(format!(
                    "the archive entry {raw:?} would write outside the bundle directory"
                )));
            }
        }
    }
    Ok(root.join(candidate))
}

fn extract_to(source: &Path, staging: &Path) -> Result<(), BundleError> {
    let file = std::fs::File::open(source)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| BundleError::Archive(e.to_string()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| BundleError::Archive(e.to_string()))?;
        let raw = entry.name().to_string();
        let out = safe_entry_path(staging, &raw)?;

        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut sink = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut sink)?;
    }
    Ok(())
}

fn read_manifest(staging: &Path) -> Result<McpbManifest, BundleError> {
    let path = staging.join("manifest.json");
    if !path.is_file() {
        return Err(BundleError::MissingManifest);
    }
    parse_manifest(&std::fs::read_to_string(path)?)
}

/// Install `source` into `local_servers_dir/<manifest name>/`.
///
/// Every failure path removes the staging directory before returning, so a
/// refused bundle is indistinguishable on disk from one never offered.
pub fn install_bundle(
    source: &Path,
    local_servers_dir: &Path,
) -> Result<InstalledBundle, BundleError> {
    std::fs::create_dir_all(local_servers_dir)?;
    let staging = local_servers_dir.join(format!(".staging-{}", std::process::id()));
    std::fs::remove_dir_all(&staging).ok();
    std::fs::create_dir_all(&staging)?;

    let outcome = (|| {
        extract_to(source, &staging)?;
        let manifest = read_manifest(&staging)?;
        // Before the name touches a path join. `validate_bundle` validates it
        // too, but that runs after `local_servers_dir.join(&manifest.name)` and the
        // `exists()` probe below — so a name like `../../../Windows` would get
        // an arbitrary-path existence check and name itself in the error.
        // Nothing is written with an unvalidated name either way; this keeps
        // "a local server name never reaches a filesystem path unvalidated" absolute
        // rather than incidental.
        crate::validate_server_name(&manifest.name)
            .map_err(|e| BundleError::Name(e.to_string()))?;
        let final_dir = local_servers_dir.join(&manifest.name);
        if final_dir.exists() {
            return Err(BundleError::AlreadyInstalled(manifest.name.clone()));
        }
        // Validated against the staging path, then re-resolved against the
        // final one: the launch command embeds the directory, and staging
        // is not where the local server will live.
        validate_bundle(&manifest, current_platform(), &staging)?;
        Ok((manifest, final_dir))
    })();

    let (manifest, final_dir) = match outcome {
        Ok(v) => v,
        Err(e) => {
            std::fs::remove_dir_all(&staging).ok();
            return Err(e);
        }
    };

    if let Err(e) = std::fs::rename(&staging, &final_dir) {
        std::fs::remove_dir_all(&staging).ok();
        return Err(BundleError::Io(e));
    }

    // Re-validating at the final location is not paranoia: it is what
    // produces the launch whose paths point where the local server now lives.
    let launch = match validate_bundle(&manifest, current_platform(), &final_dir) {
        Ok(l) => l,
        Err(e) => {
            std::fs::remove_dir_all(&final_dir).ok();
            return Err(e);
        }
    };

    Ok(InstalledBundle {
        name: manifest.name,
        version: manifest.version,
        dir: final_dir,
        launch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00rest-of-a-binary";

    /// Builds a real `.mcpb` on disk — a zip is the input format, so a fake
    /// would test the wrong thing.
    fn write_mcpb(label: &str, manifest: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("uia-mcpb-src-{}-{label}.mcpb", std::process::id()));
        std::fs::remove_file(&path).ok();
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        zip.start_file("manifest.json", opts).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        for (name, bytes) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn local_servers_dir(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("uia-mcpb-dest-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    const GOOD_MANIFEST: &str = r#"{
        "manifest_version": "0.2",
        "name": "clock",
        "version": "1.4.0",
        "server": {
            "type": "binary",
            "entry_point": "server/clock",
            "mcp_config": { "command": "${__dirname}/server/clock", "args": ["--stdio"] }
        }
    }"#;

    #[test]
    fn a_valid_binary_bundle_installs_under_its_own_name() {
        let src = write_mcpb("ok", GOOD_MANIFEST, &[("server/clock", ELF)]);
        let dest = local_servers_dir("ok");

        let installed = install_bundle(&src, &dest).unwrap();

        let name = installed.name.clone();
        let version = installed.version.clone();
        let landed = dest.join("clock").join("server").join("clock").is_file();
        let command_ok = installed.launch.command.ends_with("server/clock");
        std::fs::remove_file(&src).ok();
        std::fs::remove_dir_all(&dest).ok();

        assert_eq!(name, "clock");
        assert_eq!(version.as_deref(), Some("1.4.0"));
        assert!(landed, "the bundle's files did not land under <dest>/clock");
        assert!(command_ok, "got {}", installed.launch.command);
    }

    /// The install must be all-or-nothing: a half-extracted rejected bundle
    /// on disk is a local server nobody approved sitting where approved ones live.
    #[test]
    fn a_rejected_bundle_leaves_nothing_in_the_install_directory() {
        let manifest = r#"{
            "manifest_version": "0.2",
            "name": "sneaky",
            "version": "1.0.0",
            "server": {
                "type": "node",
                "entry_point": "server/index.js",
                "mcp_config": { "command": "node", "args": ["${__dirname}/server/index.js"] }
            }
        }"#;
        let src = write_mcpb("node", manifest, &[("server/index.js", b"console.log(1)")]);
        let dest = local_servers_dir("node");

        let err = install_bundle(&src, &dest).unwrap_err();

        // Check for ANY leftover entry, not just <dest>/sneaky: a rejected
        // bundle fails validation before the rename ever creates a
        // <manifest-name> directory, so a `sneaky`-only check can never see
        // the thing that actually needs cleaning up — the hidden
        // `.staging-{pid}` directory holding the half-extracted files. If
        // the staging-cleanup call were ever removed, that directory would
        // linger here and this assertion is what would catch it.
        let leftover_entries: Vec<_> = std::fs::read_dir(&dest)
            .map(|entries| entries.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        std::fs::remove_file(&src).ok();
        std::fs::remove_dir_all(&dest).ok();

        assert!(
            matches!(&err, BundleError::NotBinary(t) if t == "node"),
            "got {err:?}"
        );
        assert!(
            leftover_entries.is_empty(),
            "a refused bundle left {} entries behind in the install directory \
             (including any hidden .staging-* dir): {leftover_entries:?}",
            leftover_entries.len()
        );
    }

    #[test]
    fn an_archive_without_a_manifest_is_refused() {
        let path = std::env::temp_dir().join(format!(
            "uia-mcpb-src-{}-nomanifest.mcpb",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("readme.txt", opts).unwrap();
            zip.write_all(b"nothing here").unwrap();
            zip.finish().unwrap();
        }
        let dest = local_servers_dir("nomanifest");

        let err = install_bundle(&path, &dest).unwrap_err();

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dest).ok();
        assert!(matches!(err, BundleError::MissingManifest), "got {err:?}");
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_refused_as_an_archive_error() {
        let path =
            std::env::temp_dir().join(format!("uia-mcpb-src-{}-notzip.mcpb", std::process::id()));
        std::fs::write(&path, b"this is not a zip file").unwrap();
        let dest = local_servers_dir("notzip");

        let err = install_bundle(&path, &dest).unwrap_err();

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(&dest).ok();
        assert!(matches!(err, BundleError::Archive(_)), "got {err:?}");
    }

    /// Zip-slip: an entry named `../` writes outside the directory being
    /// extracted into. Rejecting it during extraction matters even though
    /// `validate_bundle` also checks containment — by then the file is
    /// already written.
    #[test]
    fn an_entry_escaping_the_extraction_directory_is_refused() {
        let src = write_mcpb(
            "slip",
            GOOD_MANIFEST,
            &[("server/clock", ELF), ("../escaped.txt", b"pwned")],
        );
        let dest = local_servers_dir("slip");

        let err = install_bundle(&src, &dest).unwrap_err();

        let escaped = dest
            .parent()
            .map(|p| p.join("escaped.txt").exists())
            .unwrap_or(false);
        std::fs::remove_file(&src).ok();
        std::fs::remove_dir_all(&dest).ok();

        assert!(matches!(err, BundleError::Archive(_)), "got {err:?}");
        assert!(
            !escaped,
            "a ../ entry wrote outside the extraction directory"
        );
    }

    /// Reinstalling over an existing local server silently would make "what is
    /// installed" and "what was approved" drift apart.
    #[test]
    fn installing_a_second_bundle_with_the_same_name_is_refused() {
        let src = write_mcpb("dup", GOOD_MANIFEST, &[("server/clock", ELF)]);
        let dest = local_servers_dir("dup");

        install_bundle(&src, &dest).unwrap();
        let err = install_bundle(&src, &dest).unwrap_err();

        std::fs::remove_file(&src).ok();
        std::fs::remove_dir_all(&dest).ok();
        assert!(
            matches!(&err, BundleError::AlreadyInstalled(n) if n == "clock"),
            "got {err:?}"
        );
    }
}

// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Installable local MCP server bundles (`.mcpb`).
//!
//! A `.mcpb` is a zip carrying a `manifest.json` (Anthropic's MCPB/DXT
//! schema) plus the files the server needs. uia installs one only if it
//! ships a compiled executable: a bundle that hands us an interpreter and
//! source is arbitrary code, and the point of requiring a bundle at all is
//! that what runs was reviewable before it ran.
//!
//! Split three ways on purpose. `manifest` is pure parsing and template
//! substitution, `validate` is the trust decision, and `install` is the
//! only part that touches the filesystem — so the rules that matter are
//! testable without building a zip.

pub mod install;
pub mod manifest;
pub mod validate;

pub use install::{InstalledBundle, install_bundle};
pub use manifest::{
    McpbManifest, McpbMcpConfig, McpbServer, ResolvedLaunch, current_platform, parse_manifest,
    resolve_launch,
};
pub use validate::{SCRIPT_EXTENSIONS, validate_bundle};

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("bundle not readable: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a valid .mcpb archive: {0}")]
    Archive(String),
    #[error("the bundle has no manifest.json")]
    MissingManifest,
    #[error("manifest.json is not valid JSON: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error(
        "this bundle declares server.type {0:?}, but uia installs only \"binary\" bundles \
         \u{2014} a bundled interpreter running bundled source is arbitrary code, which is \
         exactly what requiring a bundle is meant to prevent"
    )]
    NotBinary(String),
    #[error("the bundle declares no command and no entry point, so there is nothing to run")]
    NoCommand,
    #[error("the bundle's launch command {0:?} is a .{1} script, not a compiled executable")]
    ScriptCommand(String, String),
    #[error(
        "the bundle's launch command {0:?} begins with a #! line, so it is a script, \
         not a compiled executable"
    )]
    ShebangCommand(String),
    #[error(
        "the bundle's launch command {0:?} resolves outside the bundle directory; a local server \
         may only run a file it shipped"
    )]
    EscapesBundle(String),
    #[error("the bundle's launch command {0:?} is not a file inside the bundle")]
    MissingCommand(String),
    #[error("invalid local server name: {0}")]
    Name(String),
    #[error("a local server named {0:?} is already installed")]
    AlreadyInstalled(String),
}

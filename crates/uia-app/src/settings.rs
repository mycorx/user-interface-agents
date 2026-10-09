// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Persists the user's engine choice across restarts, separately from
//! [`crate::config::Config`].
//!
//! `Config` is deliberately load-only (S12's `Deserialize`-only sections):
//! `OpenAiSection`/`BedrockSection` carry credentials, and giving the whole
//! struct a save path would mean a UI-driven write risks serialising a key
//! back to disk. This is a small, separate JSON file holding nothing but the
//! engine choice, so a UI write can never touch a secret.
//!
//! Restart-to-switch, not live switching (S18; PLAN.md amended): the
//! persisted choice is read once at startup (`resolve_engine_choice`) and
//! applied before `build_session` connects. Changing it while a session is
//! running has no effect until the app restarts.

use crate::config::{AudioSection, EngineChoice};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct EngineSettings {
    pub engine: EngineChoice,
}

/// Foundry's `endpoint` is not a secret (`FoundrySection::endpoint`'s own doc
/// comment: "no OS-keystore tier for this field"), so it can't go through
/// `secrets.rs`'s `set_secret`/`get_secret_status`. It also can't be
/// UI-written back into `uia.toml` directly - `Config` is deliberately
/// load-only, see this module's own top comment - so it gets the same
/// sidecar-file treatment as the engine choice: a small JSON file next to
/// `uia.toml` that only the UI ever writes.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct FoundrySettings {
    pub endpoint: Option<String>,
}

/// `true` for a well-formed `http://`/`https://` URL with a non-empty host
/// and no embedded whitespace - not a full RFC 3986 parse (no `url` crate
/// dependency for one field), just enough to reject the obvious typos this
/// field is actually at risk of: a bare hostname with no scheme, a pasted
/// value with a stray space or newline, or `https://` with nothing after it.
pub fn is_valid_http_url(value: &str) -> bool {
    if value.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    let host = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    matches!(host, Some(h) if !h.is_empty())
}

/// Same sidecar-file treatment as `EngineSettings`/`FoundrySettings`, for the
/// Settings UI's Audio tab. Unlike the two `*_device` fields (an override of
/// which physical device to use, `None` meaning "trust the OS default"),
/// `aec_enabled`/`barge_in_threshold` are themselves `Option` here only to
/// distinguish "this sidecar file has never set this field" from "the UI
/// explicitly chose a value" - `None` means fall through to whatever
/// `uia.toml` says, not "disable" or "zero".
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct AudioSettings {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub aec_enabled: Option<bool>,
    pub barge_in_threshold: Option<f32>,
}

/// Same sidecar-file treatment as the others, for the redesigned Settings'
/// General tab (PLAN.md Stage 7): assistant identity (`name`) and
/// app-lifecycle behavior (`always_on_top`/`quit_on_close`). No `uia.toml`
/// counterpart to merge with — like `UiSettings`, these are pure UI-set
/// values with no config-file tier, so there is no `resolve_*` function,
/// only a default-filled load.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct AgentSettings {
    pub name: Option<String>,
    pub always_on_top: Option<bool>,
    pub quit_on_close: Option<bool>,
    /// Whether to keep a local transcript of the conversation. `None` defers
    /// to `uia.toml`'s `[memory] backend`, so a config file that says `none`
    /// is not silently overridden by a UI that has never been touched.
    pub memory_enabled: Option<bool>,
    /// A place name or `lat,lon`, handed to a locally-spawned `open-meteo-mcp`
    /// as `OPEN_METEO_DEFAULT_LOCATION` (see `session::mcp_targets`). `None`
    /// means unconfigured — the weather server then asks the user rather than
    /// guessing, which is the server's own deliberate default and is not
    /// second-guessed here. `#[serde(default)]` is belt-and-braces here (a
    /// missing `Option<T>` key already deserializes to `None`) but documents
    /// the intent that a sidecar written before this field existed must keep
    /// loading rather than being rejected.
    #[serde(default)]
    pub home_location: Option<String>,
    /// Whether the app checks for updates in the background. `None` (never
    /// touched) means on; the manual "Check now" works either way.
    #[serde(default)]
    pub auto_update_check: Option<bool>,
}

/// `uia-agent.json`, same sibling-of-`uia.toml` convention as
/// [`settings_path_for`].
pub fn agent_settings_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-agent.json")
}

/// Accepted image extensions for [`copy_agent_avatar`] — matched
/// case-insensitively against whatever the OS file-picker handed back.
///
/// No `bmp`: it is uncompressed, so an ordinary 4000x3000 image is ~36 MB and
/// would hit [`MAX_AVATAR_BYTES`] every time, and nothing exports an avatar as
/// BMP deliberately. No `svg` either, and not for size — it can carry scripts
/// and external references, which is the wrong shape for a file this app
/// copies into its own config directory. `heic` and `tiff` are absent because
/// the webview cannot decode them: they would copy cleanly and then render
/// broken, which is the worst failure to debug because it looks like our bug.
pub const AVATAR_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "avif"];

/// The longest edge an avatar is stored at.
///
/// The frames are 80px (HUD), 64px (editor header) and 26px (list row), so
/// this covers the largest of them at 3x DPI with room to spare. Storing a
/// 12-megapixel phone photo to fill an 80px square costs a full decode on
/// every HUD launch and megabytes of config directory, for pixels nothing
/// will ever show.
pub const MAX_AVATAR_EDGE: u32 = 512;

/// The ceiling on a file that cannot be downscaled — animated or otherwise
/// not re-encodable here. Generous enough for a real animated avatar and
/// small enough that nothing pathological lands in the config directory.
pub const MAX_AVATAR_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AvatarError {
    #[error("{0:?} has no file extension")]
    NoExtension(PathBuf),
    #[error("{0:?} is not a supported image type (expected one of {AVATAR_EXTENSIONS:?})")]
    UnsupportedExtension(String),
    #[error("could not copy avatar: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0:?} is not a valid persona id")]
    InvalidId(String),
    #[error(
        "that image is {mb:.1} MB, over the {limit} MB limit. Animated images are \
         kept as they are rather than shrunk, so pick a smaller one."
    )]
    TooLarge { mb: f64, limit: u64 },
    #[error("that image could not be read: {0}")]
    Decode(String),
}

/// Where a persona's avatar lives. App-derived from a validated id, never
/// from anything the frontend supplies — the one rule that survives the move
/// of personas out of their own Markdown file and into a JSON sidecar.
pub fn persona_avatar_path_for(config_path: &Path, id: &str, ext: &str) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("uia-agent-avatar-{id}.{ext}"))
}

/// Adopt the pre-personas avatar onto `id`, if there is one to adopt.
///
/// Before personas were a list there was one avatar, written beside the config
/// as `uia-agent-avatar.<ext>` with no id in the name. Nothing reads that file
/// any more, so an install that predates the list loses the face in its HUD on
/// upgrade with nothing to explain it. The spec's "no migration" was argued
/// from "there are no existing installs", which is simply not true of a
/// machine that has been running this app -- and the spec itself notes that
/// adopting in place would have been the compatible move.
///
/// Only the image. The old free-text persona is deliberately left alone: it is
/// one blob of prose with no honest split into archetype/manner/use_when, and
/// a wrong guess is worse than retyping it.
///
/// Returns the extension the avatar now lives under, for `avatar_ext`.
/// Idempotent: an avatar already sitting under `id` wins and is reported
/// without copying anything, so repeat calls cost two stats and never clobber
/// a picture the user has since chosen.
pub fn adopt_legacy_avatar(config_path: &Path, id: &str) -> Option<String> {
    if !crate::personas::is_valid_id(id) {
        return None;
    }
    if let Some(ext) = AVATAR_EXTENSIONS
        .iter()
        .find(|ext| persona_avatar_path_for(config_path, id, ext).exists())
    {
        return Some((*ext).to_string());
    }
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let (source, ext) = AVATAR_EXTENSIONS
        .iter()
        .map(|ext| (dir.join(format!("uia-agent-avatar.{ext}")), *ext))
        .find(|(path, _)| path.exists())?;
    std::fs::copy(&source, persona_avatar_path_for(config_path, id, ext)).ok()?;
    Some(ext.to_string())
}

/// Validates `id` and `source`'s extension, copies the image beside
/// `config_path` as `uia-agent-avatar-<id>.<ext>` (replacing that persona's
/// previous avatar, extension included), and returns the destination path
/// for `set_persona_avatar` to persist into `Persona::avatar_ext`. Pure
/// path/IO logic, no `tauri::command` glue, so it is testable without a
/// running app.
pub fn copy_agent_avatar(
    config_path: &Path,
    id: &str,
    source: &Path,
) -> Result<PathBuf, AvatarError> {
    // Before any path is built, not after: the whole point of the id rules
    // is that an unvalidated id never becomes a filename.
    if !crate::personas::is_valid_id(id) {
        return Err(AvatarError::InvalidId(id.to_string()));
    }
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| AvatarError::NoExtension(source.to_path_buf()))?;
    let ext_lower = ext.to_ascii_lowercase();
    if !AVATAR_EXTENSIONS.contains(&ext_lower.as_str()) {
        return Err(AvatarError::UnsupportedExtension(ext.to_string()));
    }
    let dest = persona_avatar_path_for(config_path, id, &ext_lower);
    // Drop this persona's stale avatar under a *different* extension so
    // switching image types doesn't leave the old file orphaned. Scoped to
    // this id: another persona's avatar is not ours to delete.
    for other_ext in AVATAR_EXTENSIONS.iter().filter(|e| **e != ext_lower) {
        let stale = persona_avatar_path_for(config_path, id, other_ext);
        let _ = std::fs::remove_file(stale);
    }
    let bytes = std::fs::read(source)?;
    std::fs::write(&dest, store_as(&bytes, &ext_lower)?)?;
    Ok(dest)
}

/// Does this PNG carry animation frames?
///
/// APNG rides on the `.png` extension, so the extension cannot decide it. The
/// marker is an `acTL` chunk, which a valid APNG must place before the first
/// `IDAT`; searching only that prefix keeps a file that merely contains those
/// four bytes in its pixel data from being mistaken for one.
fn is_animated_png(bytes: &[u8]) -> bool {
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let before_data = find(bytes, b"IDAT").unwrap_or(bytes.len());
    find(&bytes[..before_data], b"acTL").is_some()
}

/// What actually gets written for an avatar: the original bytes, or a
/// downscaled re-encode of them.
///
/// Split by what can be shrunk without losing something. JPEG and still PNG
/// are re-encoded, because that is where oversized avatars come from — phone
/// photos and screenshots — and nothing is lost by storing 512px of an image
/// that will be shown at 80. GIF, WebP, AVIF and APNG are stored exactly as
/// given: all four can be animated, and pushing them through a still-image
/// decoder would silently keep frame one and throw the rest away. They get a
/// size ceiling instead, which is the only lever left when re-encoding is off
/// the table.
fn store_as<'a>(bytes: &'a [u8], ext: &str) -> Result<std::borrow::Cow<'a, [u8]>, AvatarError> {
    let format = match ext {
        "png" if !is_animated_png(bytes) => image::ImageFormat::Png,
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        _ => {
            let len = bytes.len() as u64;
            if len > MAX_AVATAR_BYTES {
                return Err(AvatarError::TooLarge {
                    mb: len as f64 / (1024.0 * 1024.0),
                    limit: MAX_AVATAR_BYTES / (1024 * 1024),
                });
            }
            return Ok(std::borrow::Cow::Borrowed(bytes));
        }
    };

    let image = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| AvatarError::Decode(e.to_string()))?;
    if image.width().max(image.height()) <= MAX_AVATAR_EDGE {
        // Already small enough. Returning the original rather than a re-encode
        // keeps a lossy format from losing a generation for no reason.
        return Ok(std::borrow::Cow::Borrowed(bytes));
    }

    // `resize` fits *within* the box and preserves aspect ratio, so passing
    // the limit for both axes scales by whichever edge is longer.
    let scaled = image.resize(
        MAX_AVATAR_EDGE,
        MAX_AVATAR_EDGE,
        image::imageops::FilterType::Lanczos3,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    scaled
        .write_to(&mut out, format)
        .map_err(|e| AvatarError::Decode(e.to_string()))?;
    Ok(std::borrow::Cow::Owned(out.into_inner()))
}

/// Same sidecar-file treatment as the others, for the redesigned Settings'
/// Providers tab (FD3): the OpenAI model id, the Bedrock model id and the
/// Foundry deployment name.
///
/// FD3 originally gave Bedrock no field here, on the grounds that Nova Sonic
/// had exactly one model id and so nothing to choose between. That is still
/// true of the catalog, but it made Bedrock the one engine whose model was
/// unexpressible in config — a second Sonic id would have been a code change,
/// and the Providers tab showed a dead static label where the other two
/// engines had controls. Bedrock now carries a field like the others; the
/// one-entry catalog lives in the UI, not in the shape of this struct.
///
/// All three fields are `Option` for the same reason as
/// [`FoundrySettings::endpoint`]: `None` means "this sidecar has never set
/// it," not "empty."
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct ModelSettings {
    pub openai: Option<String>,
    #[serde(default)]
    pub bedrock: Option<String>,
    pub foundry: Option<String>,
}

/// `uia-model.json`, same sibling-of-`uia.toml` convention as
/// [`settings_path_for`].
pub fn model_settings_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-model.json")
}

/// Same precedence rule as [`resolve_foundry_endpoint`]: the sidecar's
/// `openai` field (UI-set) wins over `uia.toml`'s `[openai] model`.
pub fn resolve_openai_model(configured: String, persisted: Option<&ModelSettings>) -> String {
    persisted
        .and_then(|s| s.openai.clone())
        .unwrap_or(configured)
}

/// Same precedence rule as [`resolve_openai_model`], for `uia.toml`'s
/// `[bedrock] model`.
pub fn resolve_bedrock_model(configured: String, persisted: Option<&ModelSettings>) -> String {
    persisted
        .and_then(|s| s.bedrock.clone())
        .unwrap_or(configured)
}

/// Same precedence rule as [`resolve_openai_model`], for `uia.toml`'s
/// `[foundry] deployment` — FD3's free-text field, not a fixed catalog id.
pub fn resolve_foundry_model(configured: String, persisted: Option<&ModelSettings>) -> String {
    persisted
        .and_then(|s| s.foundry.clone())
        .unwrap_or(configured)
}

/// Same sidecar-file treatment as the others, for the redesigned Settings'
/// Visual tab (S9): `theme`/`accent`. No `uia.toml` counterpart to merge
/// with — like `AgentSettings`, these are pure UI-set values with no
/// config-file tier, so there is no `resolve_*` function, only a
/// default-filled load. `theme` of `None` means "follow the OS" — the
/// frontend, not this file, supplies the `prefers-color-scheme` fallback,
/// since that is a browser-side query with no meaningful Rust equivalent.
/// `glass_tint` (S11): `None`/`"rim"` keeps the accent to the card's
/// border/edge-light only (FD18's default); `"wash"` additionally blends a
/// low-opacity accent tint into the glass fill itself.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Default)]
pub struct UiSettings {
    pub theme: Option<String>,
    pub accent: Option<String>,
    pub glass_tint: Option<String>,
}

/// `uia-ui.json`, same sibling-of-`uia.toml` convention as
/// [`settings_path_for`].
pub fn ui_settings_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-ui.json")
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings not writable: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
}

/// `uia-engine.json`, next to `uia.toml` — same directory, sibling
/// name, so both files move together if a user relocates their config.
pub fn settings_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-engine.json")
}

/// `uia-foundry.json`, same sibling-of-`uia.toml` convention as
/// [`settings_path_for`], kept in its own file rather than folded into
/// `EngineSettings` - the two are unrelated UI-set values with independent
/// lifecycles, and a shared struct would make either one's shape a breaking
/// change for the other.
pub fn foundry_settings_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-foundry.json")
}

/// `uia-audio.json`, same sibling-of-`uia.toml` convention as
/// [`settings_path_for`].
pub fn audio_settings_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uia-audio.json")
}

/// `None` when the file does not exist or fails to parse — a missing or
/// corrupt settings file must never block startup, unlike a malformed
/// `uia.toml`, which `Config::load` treats as fatal. The engine choice is
/// UI-set convenience, not a hand-authored config a typo in should punish.
pub fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save<T: Serialize>(path: &Path, settings: &T) -> Result<(), SettingsError> {
    let raw = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, raw)?;
    Ok(())
}

/// The persisted choice wins when present; otherwise `uia.toml`'s
/// `engine.default`. Kept as its own function — no I/O, trivially testable —
/// so the precedence rule survives review independent of the file format.
pub fn resolve_engine_choice(
    configured_default: EngineChoice,
    persisted: Option<EngineSettings>,
) -> EngineChoice {
    persisted.map(|s| s.engine).unwrap_or(configured_default)
}

/// Same precedence rule as [`resolve_engine_choice`]: the sidecar file (UI-set)
/// wins over `uia.toml`'s inline `[foundry] endpoint`, falling back to
/// `None` when neither is set.
pub fn resolve_foundry_endpoint(
    configured: Option<String>,
    persisted: Option<FoundrySettings>,
) -> Option<String> {
    persisted.and_then(|s| s.endpoint).or(configured)
}

/// Same precedence rule as [`resolve_engine_choice`]/[`resolve_foundry_endpoint`]
/// applied field-by-field: each sidecar field wins over `uia.toml`'s
/// `[audio]` value when the UI has set it, otherwise `configured`'s value
/// (already `AudioSection::default`-filled by `Config::load`) passes through
/// unchanged. Returns a full `AudioSection` — the same struct `build_session`
/// already reads — so applying the result is one field-for-field
/// `config.audio = ...` at startup, not a second code path through the audio
/// stack.
/// `available` is this machine's enumerated device names (input and output
/// together -- a name only ever matches one side in practice). A persisted
/// choice naming nothing in that list is dropped, so a device that has been
/// unplugged, replaced or renamed degrades to the OS default instead of
/// reaching `uia_audio`'s deliberate "unmatched device is an error" path,
/// which fails the whole session and, in a packaged build with no console,
/// fails it silently.
///
/// That strictness is right for a name the user just typed into `uia.toml`
/// and got wrong. It is wrong for a name the UI persisted months ago against
/// hardware that has since changed, which is the case this filter covers.
///
/// Matching is `uia_audio::device_name_matches`, the same rule the lookup
/// itself uses. Using anything stricter here -- equality, say -- would drop
/// working settings: the lookup finds "Webcam" inside "Microphone (HD Pro
/// Webcam C920)", and a filter that did not would silently move a user off a
/// device that was about to work.
///
/// An empty `available` disables filtering: enumeration failed, and throwing
/// away every setting on the strength of a failed listing would be worse than
/// the problem being solved.
/// The General tab's memory toggle wins over `uia.toml`'s `[memory] backend`
/// when it has been set, same field-by-field precedence as the audio and
/// engine sidecars: the UI is the more recent statement of intent.
///
/// Untouched (`None`) defers to the config file, so `backend = "none"` is
/// still honoured on a machine whose UI has never been opened.
pub fn resolve_memory_enabled(
    configured: crate::config::MemoryBackend,
    persisted: Option<bool>,
) -> bool {
    persisted.unwrap_or(configured == crate::config::MemoryBackend::Local)
}

pub fn resolve_auto_update_check(persisted: Option<bool>) -> bool {
    persisted.unwrap_or(true)
}

pub fn resolve_audio_settings(
    configured: &AudioSection,
    persisted: Option<AudioSettings>,
    available: &[String],
) -> AudioSection {
    let Some(persisted) = persisted else {
        return configured.clone();
    };
    let still_present = |name: Option<String>| -> Option<String> {
        let name = name?;
        if available.is_empty()
            || available
                .iter()
                .any(|a| uia_audio::device_name_matches(a, &name))
        {
            return Some(name);
        }
        eprintln!(
            "uia: persisted audio device {name:?} is not present on this machine; \
             falling back to the default device"
        );
        None
    };
    AudioSection {
        aec_enabled: persisted.aec_enabled.unwrap_or(configured.aec_enabled),
        input_device: still_present(persisted.input_device)
            .or_else(|| configured.input_device.clone()),
        output_device: still_present(persisted.output_device)
            .or_else(|| configured.output_device.clone()),
        barge_in_threshold: persisted
            .barge_in_threshold
            .or(configured.barge_in_threshold),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_engine_choice_survives_reload() {
        let dir =
            std::env::temp_dir().join(format!("uia-settings-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-engine.json");

        save(
            &path,
            &EngineSettings {
                engine: EngineChoice::Bedrock,
            },
        )
        .unwrap();
        let loaded = load::<EngineSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(EngineSettings {
                engine: EngineChoice::Bedrock
            })
        );
    }

    #[test]
    fn a_missing_settings_file_loads_as_none_not_an_error() {
        let path =
            std::env::temp_dir().join(format!("uia-settings-missing-{}.json", std::process::id()));
        assert_eq!(load::<EngineSettings>(&path), None);
    }

    #[test]
    fn a_corrupt_settings_file_loads_as_none_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("uia-settings-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-engine.json");
        std::fs::write(&path, "{ not json").unwrap();

        let loaded = load::<EngineSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(loaded, None);
    }

    #[test]
    fn settings_path_is_a_sibling_of_the_config_file() {
        let config_path = Path::new("/home/user/uia.toml");
        assert_eq!(
            settings_path_for(config_path),
            Path::new("/home/user/uia-engine.json")
        );
    }

    #[test]
    fn the_persisted_choice_overrides_the_configured_default() {
        assert_eq!(
            resolve_engine_choice(
                EngineChoice::OpenAi,
                Some(EngineSettings {
                    engine: EngineChoice::Bedrock
                })
            ),
            EngineChoice::Bedrock
        );
    }

    #[test]
    fn no_persisted_choice_falls_back_to_the_configured_default() {
        assert_eq!(
            resolve_engine_choice(EngineChoice::Bedrock, None),
            EngineChoice::Bedrock
        );
    }

    #[test]
    fn a_saved_foundry_endpoint_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "uia-foundry-settings-roundtrip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-foundry.json");

        save(
            &path,
            &FoundrySettings {
                endpoint: Some("https://example.openai.azure.com".into()),
            },
        )
        .unwrap();
        let loaded = load::<FoundrySettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(FoundrySettings {
                endpoint: Some("https://example.openai.azure.com".into())
            })
        );
    }

    #[test]
    fn foundry_settings_path_is_a_sibling_of_the_config_file() {
        let config_path = Path::new("/home/user/uia.toml");
        assert_eq!(
            foundry_settings_path_for(config_path),
            Path::new("/home/user/uia-foundry.json")
        );
    }

    #[test]
    fn the_persisted_foundry_endpoint_overrides_the_configured_one() {
        assert_eq!(
            resolve_foundry_endpoint(
                Some("https://configured.example.com".into()),
                Some(FoundrySettings {
                    endpoint: Some("https://ui-set.example.com".into())
                })
            ),
            Some("https://ui-set.example.com".into())
        );
    }

    #[test]
    fn no_persisted_foundry_endpoint_falls_back_to_the_configured_one() {
        assert_eq!(
            resolve_foundry_endpoint(Some("https://configured.example.com".into()), None),
            Some("https://configured.example.com".into())
        );
    }

    #[test]
    fn valid_http_and_https_urls_are_accepted() {
        assert!(is_valid_http_url("https://your-resource.openai.azure.com"));
        assert!(is_valid_http_url("http://localhost:9000"));
        assert!(is_valid_http_url("HTTPS://Example.Com"));
    }

    #[test]
    fn a_url_missing_a_scheme_is_rejected() {
        assert!(!is_valid_http_url("your-resource.openai.azure.com"));
    }

    #[test]
    fn a_url_with_no_host_after_the_scheme_is_rejected() {
        assert!(!is_valid_http_url("https://"));
        assert!(!is_valid_http_url("http://"));
    }

    #[test]
    fn a_url_with_embedded_whitespace_is_rejected() {
        assert!(!is_valid_http_url("https://example.com/ path"));
        assert!(!is_valid_http_url("https://example.com\n"));
    }

    #[test]
    fn an_unsupported_scheme_is_rejected() {
        assert!(!is_valid_http_url("ftp://example.com"));
    }

    /// The bug this fixes: a device that is gone -- unplugged, replaced,
    /// renamed by a driver update -- must not be handed to `build_session`.
    /// `uia_audio` treats an unmatched name as a hard error and the whole
    /// session fails; in a release build (`windows_subsystem = "windows"`)
    /// the stderr message goes nowhere, so the app just silently never works.
    #[test]
    fn a_persisted_device_absent_from_this_machine_is_dropped() {
        let configured = AudioSection::default();
        let persisted = AudioSettings {
            aec_enabled: None,
            input_device: Some("HD Pro Webcam C920".into()),
            output_device: Some("Creative Pebble X".into()),
            barge_in_threshold: None,
        };
        let available = vec!["Some Other Mic".to_string()];

        let resolved = resolve_audio_settings(&configured, Some(persisted), &available);

        assert_eq!(resolved.input_device, None, "absent input must be dropped");
        assert_eq!(
            resolved.output_device, None,
            "absent output must be dropped"
        );
    }

    /// The ordinary case must be untouched.
    #[test]
    fn a_persisted_device_present_on_this_machine_is_kept() {
        let configured = AudioSection::default();
        let persisted = AudioSettings {
            aec_enabled: None,
            input_device: Some("Some Other Mic".into()),
            output_device: None,
            barge_in_threshold: None,
        };
        let available = vec!["Some Other Mic".to_string()];

        let resolved = resolve_audio_settings(&configured, Some(persisted), &available);

        assert_eq!(resolved.input_device.as_deref(), Some("Some Other Mic"));
    }

    /// The trap. The lookup matches by case-insensitive substring, so a
    /// persisted "Webcam" really does resolve to "Microphone (HD Pro Webcam
    /// C920)". A filter using equality would call that unavailable and move
    /// the user off a device that works -- causing the exact silent switch
    /// this whole mechanism exists to avoid.
    #[test]
    fn a_partial_name_the_lookup_would_match_is_not_dropped() {
        let configured = AudioSection::default();
        let persisted = AudioSettings {
            aec_enabled: None,
            input_device: Some("Webcam".into()),
            output_device: None,
            barge_in_threshold: None,
        };
        let available = vec!["Microphone (HD Pro Webcam C920)".to_string()];

        let resolved = resolve_audio_settings(&configured, Some(persisted), &available);

        assert_eq!(resolved.input_device.as_deref(), Some("Webcam"));
    }

    /// Enumeration failed or has not run. Dropping every choice on the
    /// strength of an empty listing would be worse than the problem.
    #[test]
    fn an_empty_available_list_does_not_filter_anything() {
        let configured = AudioSection::default();
        let persisted = AudioSettings {
            aec_enabled: None,
            input_device: Some("HD Pro Webcam C920".into()),
            output_device: None,
            barge_in_threshold: None,
        };

        let resolved = resolve_audio_settings(&configured, Some(persisted), &[]);

        assert_eq!(resolved.input_device.as_deref(), Some("HD Pro Webcam C920"));
    }
    #[test]
    fn a_saved_audio_settings_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "uia-audio-settings-roundtrip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-audio.json");

        save(
            &path,
            &AudioSettings {
                input_device: Some("Webcam".into()),
                output_device: Some("Speakers".into()),
                aec_enabled: Some(false),
                barge_in_threshold: Some(0.03),
            },
        )
        .unwrap();
        let loaded = load::<AudioSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(AudioSettings {
                input_device: Some("Webcam".into()),
                output_device: Some("Speakers".into()),
                aec_enabled: Some(false),
                barge_in_threshold: Some(0.03),
            })
        );
    }

    #[test]
    fn a_saved_agent_settings_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "uia-agent-settings-roundtrip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-agent.json");

        save(
            &path,
            &AgentSettings {
                name: Some("Jarvis".into()),
                always_on_top: Some(true),
                quit_on_close: Some(false),
                memory_enabled: Some(false),
                home_location: Some("Hobart".into()),
                auto_update_check: None,
            },
        )
        .unwrap();
        let loaded = load::<AgentSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(AgentSettings {
                name: Some("Jarvis".into()),
                always_on_top: Some(true),
                quit_on_close: Some(false),
                memory_enabled: Some(false),
                home_location: Some("Hobart".into()),
                auto_update_check: None,
            })
        );
    }

    /// A sidecar written before this field existed has no `home_location`
    /// key at all. `Option<T>` already deserializes a missing key as `None`,
    /// so this is a regression guard rather than a test of `serde(default)`
    /// itself — it fails if a future change makes the field non-optional or
    /// otherwise required.
    #[test]
    fn an_agent_settings_file_from_before_home_location_existed_still_loads() {
        let dir =
            std::env::temp_dir().join(format!("uia-agent-settings-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-agent.json");
        std::fs::write(&path, r#"{"name":"Jarvis"}"#).unwrap();

        let loaded = load::<AgentSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(AgentSettings {
                name: Some("Jarvis".into()),
                home_location: None,
                ..Default::default()
            })
        );
    }

    #[test]
    fn automatic_update_checks_default_to_on() {
        assert!(resolve_auto_update_check(None));
        assert!(resolve_auto_update_check(Some(true)));
        assert!(!resolve_auto_update_check(Some(false)));
    }

    #[test]
    fn an_agent_sidecar_written_before_auto_update_check_still_loads() {
        let loaded: AgentSettings = serde_json::from_str(r#"{"name":"Ada"}"#).unwrap();
        assert_eq!(loaded.auto_update_check, None);
    }

    #[test]
    fn agent_settings_path_is_a_sibling_of_the_config_file() {
        let config_path = Path::new("/home/user/uia.toml");
        assert_eq!(
            agent_settings_path_for(config_path),
            Path::new("/home/user/uia-agent.json")
        );
    }

    #[test]
    fn copying_an_avatar_lands_beside_the_config_file_with_its_extension() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = a_png(&dir, "chosen.PNG", 64, 64);

        let dest = copy_agent_avatar(&config_path, "secretary", &source).unwrap();

        let result = (
            dest.clone(),
            std::fs::read(&dest).unwrap(),
            std::fs::read(&source).unwrap(),
        );
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(result.0, dir.join("uia-agent-avatar-secretary.png"));
        assert_eq!(result.1, result.2);
    }

    /// A real image of the given size, in the format its extension names,
    /// encoded by the same crate that will decode it.
    ///
    /// Dummy bytes will not do any more: a `.png` or `.jpg` that cannot be
    /// decoded is now refused rather than stored, which is deliberate — the
    /// alternative is a file that copies cleanly and then renders broken.
    fn a_png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> PathBuf {
        let path = dir.join(name);
        let rgba = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 120, 200, 255]));
        let image = image::DynamicImage::ImageRgba8(rgba);
        // JPEG has no alpha channel, so encoding RGBA to it fails outright.
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
        {
            Some(e) if e == "jpg" || e == "jpeg" => image.to_rgb8().save(&path).unwrap(),
            _ => image.save(&path).unwrap(),
        }
        path
    }

    #[test]
    fn a_file_that_claims_png_but_cannot_be_decoded_is_refused() {
        // The worst failure mode for an avatar is one that imports without
        // complaint and then renders as a broken image, because that looks
        // like our bug rather than a bad file. Refusing at the door is why
        // `heic` and `tiff` are absent from AVATAR_EXTENSIONS too.
        let dir = std::env::temp_dir().join(format!("uia-avatar-bogus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("not-really.png");
        std::fs::write(&source, b"this is not a png at all").unwrap();

        let err = copy_agent_avatar(&dir.join("uia.toml"), "default", &source).unwrap_err();

        let landed = dir.join("uia-agent-avatar-default.png").exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, AvatarError::Decode(_)), "got {err:?}");
        assert!(!landed, "nothing may be written when the decode fails");
    }

    #[test]
    fn an_oversized_still_is_downscaled_on_import() {
        // A phone photo fills an 80px frame exactly as well after this, and
        // costs a fraction of the decode on every HUD launch.
        let dir = std::env::temp_dir().join(format!("uia-avatar-shrink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = a_png(&dir, "huge.png", 3000, 2000);

        let dest = copy_agent_avatar(&config_path, "default", &source).unwrap();

        let (w, h) = image::image_dimensions(&dest).unwrap();
        let shrank =
            std::fs::metadata(&dest).unwrap().len() < std::fs::metadata(&source).unwrap().len();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(w, MAX_AVATAR_EDGE, "the long edge sets the scale");
        assert_eq!(h, 341, "aspect ratio is preserved, not squashed");
        assert!(shrank, "the stored file must actually be smaller");
    }

    #[test]
    fn an_image_already_within_the_limit_is_not_re_encoded() {
        // Re-encoding a small PNG would cost quality for nothing.
        let dir = std::env::temp_dir().join(format!("uia-avatar-small-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = a_png(&dir, "small.png", 128, 128);
        let original = std::fs::read(&source).unwrap();

        let dest = copy_agent_avatar(&config_path, "default", &source).unwrap();

        let stored = std::fs::read(&dest).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(stored, original, "a small image is copied byte for byte");
    }

    #[test]
    fn an_animated_gif_is_stored_untouched_so_it_keeps_animating() {
        // The whole reason resizing is format-dependent: re-encoding this
        // through a still-image decoder would silently keep frame one.
        let dir = std::env::temp_dir().join(format!("uia-avatar-gif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = dir.join("spin.gif");
        // Not a decodable GIF, deliberately: reaching the decoder at all would
        // be the bug, so the test fails loudly if the pass-through is lost.
        std::fs::write(&source, b"GIF89a fake animated bytes").unwrap();

        let dest = copy_agent_avatar(&config_path, "default", &source).unwrap();

        let stored = std::fs::read(&dest).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(stored, b"GIF89a fake animated bytes");
    }

    #[test]
    fn an_animated_png_is_left_alone_rather_than_flattened() {
        // APNG rides on the `.png` extension, so extension alone cannot decide
        // this: an `acTL` chunk before the image data is what marks it, and
        // resizing would throw every frame but the first away.
        let dir = std::env::temp_dir().join(format!("uia-avatar-apng-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = dir.join("spin.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"\0\0\0\x08acTL\0\0\0\x02\0\0\0\0crc!");
        bytes.extend_from_slice(&[0u8; 64]);
        std::fs::write(&source, &bytes).unwrap();

        let dest = copy_agent_avatar(&config_path, "default", &source).unwrap();

        let stored = std::fs::read(&dest).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(stored, bytes, "an APNG must survive import intact");
    }

    #[test]
    fn an_enormous_animated_file_is_refused_with_its_size_named() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-huge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = dir.join("enormous.gif");
        std::fs::write(&source, vec![0u8; (MAX_AVATAR_BYTES + 1) as usize]).unwrap();

        let err = copy_agent_avatar(&config_path, "default", &source).unwrap_err();

        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(err, AvatarError::TooLarge { .. }),
            "expected TooLarge, got {err:?}"
        );
        assert!(err.to_string().contains("8 MB"), "{err}");
    }

    #[test]
    fn bmp_is_no_longer_accepted_and_avif_is() {
        assert!(!AVATAR_EXTENSIONS.contains(&"bmp"));
        assert!(AVATAR_EXTENSIONS.contains(&"avif"));
        for kept in ["png", "jpg", "jpeg", "gif", "webp"] {
            assert!(AVATAR_EXTENSIONS.contains(&kept), "{kept} must stay");
        }
    }

    #[test]
    fn the_single_persona_avatar_is_adopted_onto_the_default_persona() {
        // The spec's "no migration" was argued from "there are no existing
        // installs", which turned out to be false: an install predating the
        // persona list has `uia-agent-avatar.<ext>` with no id in the name,
        // and nothing reads it any more, so the HUD's face silently vanishes
        // on upgrade. Adopting just the image is cheap and lossless; the
        // free-text persona is deliberately NOT adopted, having no honest
        // mapping onto archetype/manner/use_when.
        let dir = std::env::temp_dir().join(format!("uia-avatar-adopt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        std::fs::write(dir.join("uia-agent-avatar.jpg"), b"the old face").unwrap();

        let adopted = adopt_legacy_avatar(&config_path, "default");

        let dest = dir.join("uia-agent-avatar-default.jpg");
        let landed = dest.exists().then(|| std::fs::read(&dest).unwrap());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(adopted.as_deref(), Some("jpg"));
        assert_eq!(landed.as_deref(), Some(&b"the old face"[..]));
    }

    #[test]
    fn adoption_reports_nothing_when_there_is_no_legacy_avatar() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-noadopt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");

        let adopted = adopt_legacy_avatar(&config_path, "default");

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(adopted, None, "a fresh install has nothing to adopt");
    }

    #[test]
    fn adoption_is_idempotent_and_never_overwrites_a_chosen_avatar() {
        // `load_personas` runs this every time the sidecar is missing, and the
        // HUD reloads it per launch, so this must not re-copy on each call --
        // and must never clobber an avatar the user has since chosen.
        let dir = std::env::temp_dir().join(format!("uia-avatar-idem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        std::fs::write(dir.join("uia-agent-avatar.jpg"), b"the old face").unwrap();
        std::fs::write(dir.join("uia-agent-avatar-default.png"), b"the chosen face").unwrap();

        let adopted = adopt_legacy_avatar(&config_path, "default");

        let chosen = std::fs::read(dir.join("uia-agent-avatar-default.png")).unwrap();
        let jpg_written = dir.join("uia-agent-avatar-default.jpg").exists();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(adopted.as_deref(), Some("png"), "the existing avatar wins");
        assert_eq!(chosen, b"the chosen face");
        assert!(!jpg_written, "the legacy file must not be copied over it");
    }

    #[test]
    fn copying_an_avatar_removes_a_stale_one_under_a_different_extension() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let old_avatar = dir.join("uia-agent-avatar-secretary.png");
        std::fs::write(&old_avatar, b"old png").unwrap();
        let source = a_png(&dir, "chosen.jpg", 64, 64);

        let dest = copy_agent_avatar(&config_path, "secretary", &source).unwrap();

        let old_still_exists = old_avatar.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(dest, dir.join("uia-agent-avatar-secretary.jpg"));
        assert!(!old_still_exists);
    }

    #[test]
    fn an_avatar_with_no_extension_is_rejected() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-noext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = dir.join("chosen");
        std::fs::write(&source, b"bytes").unwrap();

        let result = copy_agent_avatar(&config_path, "secretary", &source);

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(result, Err(AvatarError::NoExtension(_))));
    }

    #[test]
    fn an_avatar_with_an_unsupported_extension_is_rejected() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-badext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("uia.toml");
        let source = dir.join("chosen.exe");
        std::fs::write(&source, b"bytes").unwrap();

        let result = copy_agent_avatar(&config_path, "secretary", &source);

        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(result, Err(AvatarError::UnsupportedExtension(_))));
    }

    #[test]
    fn an_avatar_is_named_for_its_persona() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = a_png(&dir, "pic.PNG", 64, 64);

        let dest = copy_agent_avatar(&dir.join("uia.toml"), "secretary", &src).unwrap();

        assert_eq!(dest.file_name().unwrap(), "uia-agent-avatar-secretary.png");
        assert!(dest.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn switching_extension_drops_only_that_personas_stale_file() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("uia.toml");
        let other = dir.join("uia-agent-avatar-focus.png");
        std::fs::write(&other, b"other persona").unwrap();

        let png = a_png(&dir, "a.png", 64, 64);
        copy_agent_avatar(&config, "secretary", &png).unwrap();
        let jpg = a_png(&dir, "b.jpg", 64, 64);
        copy_agent_avatar(&config, "secretary", &jpg).unwrap();

        assert!(
            !dir.join("uia-agent-avatar-secretary.png").exists(),
            "stale ext removed"
        );
        assert!(dir.join("uia-agent-avatar-secretary.jpg").exists());
        assert!(other.exists(), "another persona's avatar must be untouched");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_invalid_id_never_reaches_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("uia-avatar-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("pic.png");
        std::fs::write(&src, b"x").unwrap();

        let err = copy_agent_avatar(&dir.join("uia.toml"), "../escape", &src).unwrap_err();

        assert!(matches!(err, AvatarError::InvalidId(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn audio_settings_path_is_a_sibling_of_the_config_file() {
        let config_path = Path::new("/home/user/uia.toml");
        assert_eq!(
            audio_settings_path_for(config_path),
            Path::new("/home/user/uia-audio.json")
        );
    }

    #[test]
    fn persisted_audio_fields_override_the_configured_ones() {
        let configured = AudioSection {
            aec_enabled: true,
            input_device: Some("configured-in".into()),
            output_device: Some("configured-out".into()),
            barge_in_threshold: Some(0.05),
        };
        let persisted = AudioSettings {
            input_device: Some("ui-in".into()),
            output_device: Some("ui-out".into()),
            aec_enabled: Some(false),
            barge_in_threshold: Some(0.02),
        };
        assert_eq!(
            resolve_audio_settings(&configured, Some(persisted), &[]),
            AudioSection {
                aec_enabled: false,
                input_device: Some("ui-in".into()),
                output_device: Some("ui-out".into()),
                barge_in_threshold: Some(0.02),
            }
        );
    }

    #[test]
    fn no_persisted_audio_settings_falls_back_to_the_configured_section_untouched() {
        let configured = AudioSection {
            aec_enabled: true,
            input_device: Some("configured-in".into()),
            output_device: None,
            barge_in_threshold: None,
        };
        assert_eq!(resolve_audio_settings(&configured, None, &[]), configured);
    }

    #[test]
    fn a_saved_model_settings_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "uia-model-settings-roundtrip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-model.json");

        save(
            &path,
            &ModelSettings {
                openai: Some("gpt-realtime-2.1".into()),
                bedrock: Some("amazon.nova-2-sonic-v1:0".into()),
                foundry: Some("my-deployment".into()),
            },
        )
        .unwrap();
        let loaded = load::<ModelSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(ModelSettings {
                openai: Some("gpt-realtime-2.1".into()),
                bedrock: Some("amazon.nova-2-sonic-v1:0".into()),
                foundry: Some("my-deployment".into()),
            })
        );
    }

    #[test]
    fn model_settings_path_is_a_sibling_of_the_config_file() {
        let config_path = Path::new("/home/user/uia.toml");
        assert_eq!(
            model_settings_path_for(config_path),
            Path::new("/home/user/uia-model.json")
        );
    }

    #[test]
    fn the_persisted_openai_model_overrides_the_configured_one() {
        let persisted = ModelSettings {
            openai: Some("gpt-realtime-2.1".into()),
            bedrock: None,
            foundry: None,
        };
        assert_eq!(
            resolve_openai_model("gpt-realtime-2.1-mini".into(), Some(&persisted)),
            "gpt-realtime-2.1"
        );
    }

    #[test]
    fn no_persisted_openai_model_falls_back_to_the_configured_one() {
        assert_eq!(
            resolve_openai_model("gpt-realtime-2.1-mini".into(), None),
            "gpt-realtime-2.1-mini"
        );
    }

    #[test]
    fn the_persisted_foundry_model_overrides_the_configured_one() {
        let persisted = ModelSettings {
            openai: None,
            bedrock: None,
            foundry: Some("ui-deployment".into()),
        };
        assert_eq!(
            resolve_foundry_model("configured-deployment".into(), Some(&persisted)),
            "ui-deployment"
        );
    }

    #[test]
    fn no_persisted_foundry_model_falls_back_to_the_configured_one() {
        assert_eq!(
            resolve_foundry_model("configured-deployment".into(), None),
            "configured-deployment"
        );
    }

    #[test]
    fn the_persisted_bedrock_model_overrides_the_configured_one() {
        let persisted = ModelSettings {
            openai: None,
            bedrock: Some("amazon.nova-9-sonic-v1:0".into()),
            foundry: None,
        };
        assert_eq!(
            resolve_bedrock_model("amazon.nova-2-sonic-v1:0".into(), Some(&persisted)),
            "amazon.nova-9-sonic-v1:0"
        );
    }

    #[test]
    fn no_persisted_bedrock_model_falls_back_to_the_configured_one() {
        assert_eq!(
            resolve_bedrock_model("amazon.nova-2-sonic-v1:0".into(), None),
            "amazon.nova-2-sonic-v1:0"
        );
    }

    #[test]
    fn a_persisted_model_field_left_unset_falls_back_to_the_configured_value_for_that_field_only() {
        let persisted = ModelSettings {
            openai: Some("gpt-realtime-2.1".into()),
            bedrock: None,
            foundry: None,
        };
        assert_eq!(
            resolve_openai_model("gpt-realtime-2.1-mini".into(), Some(&persisted)),
            "gpt-realtime-2.1"
        );
        assert_eq!(
            resolve_foundry_model("configured-deployment".into(), Some(&persisted)),
            "configured-deployment"
        );
    }

    #[test]
    fn a_saved_ui_settings_survives_reload() {
        let dir =
            std::env::temp_dir().join(format!("uia-ui-settings-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uia-ui.json");

        save(
            &path,
            &UiSettings {
                theme: Some("dark".into()),
                accent: Some("violet".into()),
                glass_tint: Some("wash".into()),
            },
        )
        .unwrap();
        let loaded = load::<UiSettings>(&path);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            loaded,
            Some(UiSettings {
                theme: Some("dark".into()),
                accent: Some("violet".into()),
                glass_tint: Some("wash".into()),
            })
        );
    }

    #[test]
    fn ui_settings_path_is_a_sibling_of_the_config_file() {
        let config_path = Path::new("/home/user/uia.toml");
        assert_eq!(
            ui_settings_path_for(config_path),
            Path::new("/home/user/uia-ui.json")
        );
    }

    #[test]
    fn a_missing_ui_settings_file_loads_as_none_not_an_error() {
        let path = std::env::temp_dir().join(format!(
            "uia-ui-settings-missing-{}.json",
            std::process::id()
        ));
        assert_eq!(load::<UiSettings>(&path), None);
    }

    #[test]
    fn a_persisted_field_left_unset_falls_back_to_the_configured_value_for_that_field_only() {
        let configured = AudioSection {
            aec_enabled: true,
            input_device: Some("configured-in".into()),
            output_device: Some("configured-out".into()),
            barge_in_threshold: Some(0.05),
        };
        let persisted = AudioSettings {
            input_device: Some("ui-in".into()),
            output_device: None,
            aec_enabled: None,
            barge_in_threshold: None,
        };
        assert_eq!(
            resolve_audio_settings(&configured, Some(persisted), &[]),
            AudioSection {
                aec_enabled: true,
                input_device: Some("ui-in".into()),
                output_device: Some("configured-out".into()),
                barge_in_threshold: Some(0.05),
            }
        );
    }
}

#[cfg(test)]
mod memory_toggle_tests {
    use super::resolve_memory_enabled;
    use crate::config::MemoryBackend;

    /// Untouched UI defers to the config file, both ways. This is the case that
    /// matters: a machine whose Settings have never been opened must still
    /// honour `backend = "none"` rather than being quietly opted in.
    #[test]
    fn an_unset_toggle_defers_to_the_config_backend() {
        assert!(resolve_memory_enabled(MemoryBackend::Local, None));
        assert!(!resolve_memory_enabled(MemoryBackend::None, None));
    }

    /// Once set, the UI wins -- same precedence as the audio and engine
    /// sidecars, because it is the more recent statement of intent.
    #[test]
    fn a_set_toggle_overrides_the_config_backend() {
        assert!(!resolve_memory_enabled(MemoryBackend::Local, Some(false)));
        assert!(resolve_memory_enabled(MemoryBackend::None, Some(true)));
    }
}

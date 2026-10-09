// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

// The desktop shell entry point, and the place the whole application is
// actually run: the overlay, the global hotkey and tray (S13), plus the real
// session — audio devices, the configured engine, MCP tools — composed from
// `Config` and driven by `Session::run` (S14).
//
// **This file cannot be compiled or tested in the development sandbox.** S13
// established why: `--features desktop` fails at `libdbus-sys` long before
// webkit2gtk, and there is no root to install the ~190-package GTK/WebKit
// closure with. Everything that could be a decision therefore lives in
// `hud.rs`, `session.rs`, `config.rs` and `activation_desktop.rs`, all of
// which have real tests that run here. What is left below is `tauri` calls
// and wiring, deliberately.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::str::FromStr;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use uia_app::activation_desktop::{self, DEFAULT_ACCELERATOR, activation_event_for_menu_id};
use uia_app::config::{AudioSection, Config, EngineChoice};
use uia_app::hud::{
    CONNECTION_EVENT, ENGINE_EVENT, ERROR_EVENT, LEVEL_EVENT, LEVEL_INTERVAL, STATE_EVENT,
    TEXT_TURN_SUPPORT_EVENT, TURN_EVENT, WindowAction, connection_payload, engine_payload,
    error_payload, level_payload, state_payload, window_action_for,
};
use uia_app::mcp_registry;
use uia_app::oauth_flow;
use uia_app::secrets::SecretStore;
use uia_app::session::build_session;
use uia_app::settings::{
    self, agent_settings_path_for, audio_settings_path_for, foundry_settings_path_for,
    model_settings_path_for, settings_path_for, ui_settings_path_for,
};
use uia_app::updater::{self, CheckTrigger, UpdateStatus, UpdaterState};
use uia_core::activation::{Activation, ActivationEvent, ChannelActivation};
use uia_core::session::SessionControl;

const HUD_WINDOW: &str = "hud";
const OPEN_SETTINGS_EVENT: &str = "uia://open-settings";

/// Emitted when the background OAuth token exchange (`complete_oauth_sign_in`)
/// fails, at any of its three failure points, AFTER the Tauri command that
/// started it has already returned `NeedsAuthorization` to the frontend and
/// moved on — there is no request/response call left to report a failure
/// through. Before this event existed, a keychain failure (or any other
/// exchange error) here left the Settings form stuck on "Waiting for you to
/// finish signing in…" forever (until the 5-minute `OAUTH_BROWSER_TIMEOUT`,
/// for the one failure mode that already had one), with no indication
/// anything had gone wrong — worse than the spec's "plain inline error"
/// requirement, not merely short of it.
const OAUTH_SIGN_IN_ERROR_EVENT: &str = "uia://oauth-sign-in-error";

/// `name` lets the frontend ignore an error for a sign-in it isn't currently
/// waiting on (e.g. an abandoned attempt's background task finishing late
/// after the user already started a different one).
fn oauth_sign_in_error_payload(name: &str, message: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "message": message })
}

/// The path `set_engine` persists to — `tauri::State`-managed so the command
/// handler doesn't need to re-derive it from cwd on every call. A newtype,
/// not a bare `PathBuf`, because `tauri::State` is keyed by type and a raw
/// `PathBuf` in `.manage()` would collide with any other path someone adds
/// later.
struct SettingsPath(PathBuf);

/// The path `set_foundry_endpoint` persists to — same rationale as
/// [`SettingsPath`], kept as its own newtype rather than reusing it since the
/// two commands persist to different sidecar files (`uia-engine.json` vs
/// `uia-foundry.json`).
struct FoundrySettingsPath(PathBuf);

/// Where the conversation store lives, or `None` when the user has memory
/// turned off. Same newtype-not-`PathBuf` rationale as the paths above.
///
/// `None` is the *resolved* setting rather than "path unknown", which is what
/// lets `get_history` tell "memory is off" apart from "nothing recorded yet"
/// — two empty histories that mean entirely different things to a reader.
struct ConversationStorePath(Option<PathBuf>);

/// The handle onto the running session (`uia_core::session::control`).
/// A newtype for the same reason as the paths above — `tauri::State` is keyed
/// by type — and `.manage()`d rather than reached for through the session task
/// because that task owns the `Session` by value and never gives it back.
/// Created in `main` before the builder, so the commands below have something
/// to talk to from the first frame the webview renders, and cloned into the
/// session once `build_session` has produced one; both copies are the same
/// switch.
struct SessionControlState(SessionControl);

/// The engine choice this launch actually resolved and started a session
/// with — `tauri::State`-managed so `get_engine` can hand it back on request
/// (S21) rather than relying solely on the one-shot `ENGINE_EVENT` emitted
/// from `setup()`, which a webview that hasn't finished registering its
/// `listen()` yet can miss outright (Tauri does not buffer events for
/// listeners that attach late). A request/response command cannot race that
/// way: nothing can call it before the value exists, and the app never calls
/// it before the value is set.
struct CurrentEngine(EngineChoice);

/// `uia.toml`'s inline `[foundry] endpoint`, read once at startup - the
/// half of `resolve_foundry_endpoint`'s precedence that `get_foundry_endpoint`
/// can't re-derive on its own since `Config` itself is not `tauri::State`-
/// managed (see `get_secret_status`'s doc comment for why).
struct ConfiguredFoundryEndpoint(Option<String>);

/// The path `set_audio_settings` persists to — same rationale as
/// [`SettingsPath`]/[`FoundrySettingsPath`].
struct AudioSettingsPath(PathBuf);

/// The audio settings this launch actually resolved and opened devices with —
/// `config.audio` after [`settings::resolve_audio_settings`] has already been
/// applied (see `main`), so `get_audio_settings` hands back the same value
/// `build_session` used rather than re-deriving it. Same request/response
/// rationale as [`CurrentEngine`].
struct CurrentAudioSettings(AudioSection);

/// The path `set_agent_settings` persists to — same
/// rationale as [`SettingsPath`]/[`FoundrySettingsPath`]/[`AudioSettingsPath`].
struct AgentSettingsPath(PathBuf);

/// The path `set_model` persists to — same rationale as
/// [`SettingsPath`]/[`FoundrySettingsPath`]/[`AudioSettingsPath`].
struct ModelSettingsPath(PathBuf);

/// The OpenAI model id, Bedrock model id and Foundry deployment name this
/// launch actually resolved (FD3) — the three `config.*` fields after
/// [`settings::resolve_openai_model`]/[`settings::resolve_bedrock_model`]/
/// [`settings::resolve_foundry_model`] have already been applied (see
/// `main`), so `get_model` hands back the same values `build_session` used.
/// Same request/response rationale as [`CurrentAudioSettings`].
struct CurrentModelSettings {
    openai: String,
    bedrock: String,
    foundry: String,
}

/// The path `set_ui_settings` persists to — same rationale as
/// [`SettingsPath`]/[`FoundrySettingsPath`]/[`AudioSettingsPath`].
struct UiSettingsPath(PathBuf);

/// `uia-personas.json`. Same newtype-per-path reason as [`SettingsPath`]: a
/// bare `PathBuf` in `.manage()` collides with every other managed path.
struct PersonasPath(PathBuf);

/// `uia-mcp.json`. Same newtype-per-path reason as [`SettingsPath`]: a bare
/// `PathBuf` in `.manage()` collides with every other managed path.
struct McpRegistryPath(PathBuf);

/// How long the background OAuth-completion task waits for the browser
/// round trip to finish, once `preview_mcp_remote_server` has already
/// returned `NeedsAuthorization` and opened (or handed the frontend) the
/// authorize URL.
///
/// Deliberately its own constant, not `mcp_registry::PREVIEW_TIMEOUT`: that
/// one bounds two machine-to-machine HTTP round trips (a few seconds at
/// most, and a mistyped URL must fail fast). This one bounds a human reading
/// a consent screen and typing a password into it — five seconds would fail
/// nearly every real sign-in.
const OAUTH_BROWSER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Where installed `.mcpb` bundles live, one directory per local server.
struct McpLocalServersDir(PathBuf);

/// What each MCP server is contributing to this session.
///
/// Written on the spawned session task — by `session::build_executor` when it
/// connects, and again by the router's listing observer on every
/// `Session::connect` — and read by `list_mcp_server_health`. Not a probe: a
/// server added or enabled after startup is simply absent from the map until
/// the next launch.
struct McpHealthState(uia_app::session::McpHealth);

fn main() {
    // The release smoke test (scripts/smoke/) runs the installed binary as
    // `uia --smoke` and needs only the exit code. First of all, before config,
    // the WSL guard or Tauri: reaching this line at all is the proof that the
    // binary started and linked.
    if uia_app::smoke::is_smoke_invocation(std::env::args()) {
        std::process::exit(0);
    }

    // Before anything else, including config: the Linux .deb will be
    // launched from a WSL shell by someone who has one open, and every failure
    // that follows (GTK with no display, cpal with no capture device) names
    // something other than the actual problem. The decision itself lives in
    // `wsl`, where it is a pure function with real tests; this is only the
    // wiring. Linux-only — the Windows MSI is the supported path there, and
    // macOS cannot be WSL.
    #[cfg(target_os = "linux")]
    {
        let facts = uia_app::wsl::WslFacts::from_environment();
        if uia_app::wsl::decide(&facts.evidence()) == uia_app::wsl::WslDecision::Block {
            eprintln!("{}", uia_app::wsl::WSL_BLOCK_MESSAGE);
            std::process::exit(1);
        }
    }

    // Loaded before the UI exists so a bad config is a startup failure with a
    // readable message, not a session that connects and then does nothing. A
    // missing file is fine — every field has a default — but a malformed or
    // invalid one is not.
    //
    // Where it is loaded from is `paths`' decision, not this function's. The
    // upward search that used to live here still happens there, and still
    // matters for the same reason: the Tauri CLI launches this binary from
    // `crates/uia-app` so that `tauri.conf.json`'s relative paths resolve,
    // not from the repo root where `uia.toml` is documented to live.
    // One decision, made once: a checkout with a uia.toml keeps its files
    // beside that file; anything else -- a packaged install -- uses the
    // per-user uia-client directory. See `paths` for the precedence rules.
    let location = uia_app::paths::resolve();
    if let Err(e) = uia_app::paths::ensure_dir(&location) {
        eprintln!(
            "uia: cannot create {}: {e}",
            location.config_dir().display()
        );
    }
    let config_path = location.config_file();
    // A missing uia.toml is not an error -- every Config field defaults, and
    // in User mode there is none until the user writes one. A malformed one
    // still exits: starting on silent defaults would hide a real mistake in
    // a file the user did write.
    let mut config = if config_path.exists() {
        Config::load(&config_path).unwrap_or_else(|e| {
            eprintln!("uia: {}: {e}", config_path.display());
            std::process::exit(1);
        })
    } else {
        Config::default()
    };

    // Opportunistic, non-destructive: copies any plaintext-resolved
    // credential (env/file/inline) into the OS keystore the first time it is
    // empty, so subsequent runs prefer the keystore. Reuses this same
    // `OsKeyring` instance below rather than constructing a second one.
    let store = uia_app::secrets::OsKeyring;
    uia_app::config::migrate_plaintext_credentials_into_keystore(&config, &store);

    // Sibling of the discovered `uia.toml`, or `./uia-engine.json` when
    // no config file exists at all (every `Config` field defaults, so a
    // missing `uia.toml` is not itself fatal — the engine settings file
    // must tolerate the same case).
    let settings_path = settings_path_for(&config_path);
    // The persisted UI choice wins over `uia.toml`'s `engine.default` —
    // see `settings::resolve_engine_choice`. Read once at startup: this is
    // restart-to-switch (S18), not live switching, so nothing after this
    // point needs to watch the settings file again.
    let engine_choice =
        settings::resolve_engine_choice(config.engine.default, settings::load(&settings_path));

    let foundry_settings_path = foundry_settings_path_for(&config_path);
    let configured_foundry_endpoint = config.foundry.resolve_endpoint();
    // `engine_for` reads the endpoint from `config.foundry`, so the UI-saved
    // value in `uia-foundry.json` has to be applied to it here, as the audio
    // and model sidecars are below. Without this the Providers tab showed a
    // saved endpoint that the session never used ("no Foundry endpoint found").
    // `configured_foundry_endpoint` stays the raw `uia.toml` value:
    // `get_foundry_endpoint` applies the same precedence itself on read.
    config.foundry.endpoint = settings::resolve_foundry_endpoint(
        configured_foundry_endpoint.clone(),
        settings::load(&foundry_settings_path),
    );

    let audio_settings_path = audio_settings_path_for(&config_path);
    // The sidecar (UI-set) overrides win over `uia.toml`'s `[audio]`
    // values field-by-field — see `settings::resolve_audio_settings`. Applied
    // to `config.audio` itself, in place, before `build_session` ever reads
    // it below: no second code path through the audio stack for "did the UI
    // override this," just the one `Config` `build_session` already trusted.
    // Enumerated once here and handed to the resolver so a persisted device
    // that is no longer plugged in degrades to the OS default, rather than
    // failing the whole session in `build_session`. A failed enumeration
    // yields an empty list, which disables the filter rather than discarding
    // every audio setting the user has.
    let available_devices: Vec<String> = input_device_names()
        .unwrap_or_default()
        .into_iter()
        .chain(output_device_names().unwrap_or_default())
        .collect();
    config.audio = settings::resolve_audio_settings(
        &config.audio,
        settings::load(&audio_settings_path),
        &available_devices,
    );
    let current_audio_settings = config.audio.clone();

    let model_settings_path = model_settings_path_for(&config_path);
    // Same restart-to-switch tier as the engine choice (S18) and FD5: all
    // three model fields are baked into `build_session`, so resolving them
    // once here (rather than watching the sidecar live) matches what a
    // restart is required for.
    let persisted_model_settings: Option<settings::ModelSettings> =
        settings::load(&model_settings_path);
    let current_model_settings = CurrentModelSettings {
        openai: settings::resolve_openai_model(
            config.openai.model.clone(),
            persisted_model_settings.as_ref(),
        ),
        bedrock: settings::resolve_bedrock_model(
            config.bedrock.model.clone(),
            persisted_model_settings.as_ref(),
        ),
        foundry: settings::resolve_foundry_model(
            config.foundry.deployment.clone(),
            persisted_model_settings.as_ref(),
        ),
    };
    config.openai.model = current_model_settings.openai.clone();
    config.bedrock.model = current_model_settings.bedrock.clone();
    config.foundry.deployment = current_model_settings.foundry.clone();

    let ui_settings_path = ui_settings_path_for(&config_path);

    let mcp_registry_path = uia_app::mcp_registry::mcp_registry_path_for(&config_path);
    let mcp_local_servers_dir = uia_app::mcp_registry::mcp_local_servers_dir_for(&config_path);
    // Read once at startup, like the engine choice and every other
    // restart-to-apply setting: installing, enabling or adding a server in
    // Settings affects the next launch, not the running session.
    let mcp_registry = uia_app::mcp_registry::load(&mcp_registry_path);
    // Shared with the session task below, which is the only writer.
    let mcp_health = uia_app::session::McpHealth::default();

    let agent_settings_path = agent_settings_path_for(&config_path);
    // No `uia.toml` counterpart to merge with (like `UiSettings`) — a
    // missing sidecar just means every field defaults (name unset, not
    // always-on-top, close hides to tray).
    let agent_settings: settings::AgentSettings =
        settings::load(&agent_settings_path).unwrap_or_default();
    let always_on_top = agent_settings.always_on_top.unwrap_or(false);
    // Resolved once at startup, same restart-to-switch tier as the engine
    // choice (S18) — a live-toggling close behavior mid-session is not worth
    // the extra state plumbing yet.
    let quit_on_close = agent_settings.quit_on_close.unwrap_or(false);

    // Same sibling-of-uia.toml convention as every other sidecar, so a user
    // who deletes the uia-client folder clears their history with it.
    //
    // `None` when memory is off, which is what `build_session` reads as
    // "keep nothing" -- the General tab's toggle wins over `[memory] backend`
    // when it has been set, and defers to it when it has not.
    let memory_enabled =
        settings::resolve_memory_enabled(config.memory.backend, agent_settings.memory_enabled);
    let memory_path = memory_enabled.then(|| uia_app::memory::FileMemory::path_for(&config_path));
    // FD5: which persona the app launches as is baked into `build_session`,
    // so — same restart-to-switch tier as the engine and model choices — the
    // book is read once here rather than watched live. A `switch_persona`
    // call later in the session changes the running prompt and nothing on
    // disk, so this file is only ever rewritten by the Personas tab.
    let persona_book =
        uia_app::personas::load_personas(&uia_app::personas::personas_path_for(&config_path)).book;
    // Same restart-to-switch tier as persona (S11: previously loaded into
    // `agent_settings` for the tray/UI's own display but never actually
    // reached `build_session`, so the model never knew about it).
    let agent_name = agent_settings.name.clone();
    // Same restart-to-switch tier as `agent_name`: read once here, applied
    // once by `build_session`. Blank-vs-unset is `mcp_targets`'s job, not
    // this call site's — the raw value is passed through untouched.
    let home_location = agent_settings.home_location.clone();

    // Two channels, not one — but unlike before (PLAN.md Stage 7 decision
    // #6), `ui_tx`/`ui_rx` now drive ONLY the window (show/hide/focus), and
    // `session_tx` is used exactly once below, immediately, to start the
    // session at launch. Session activation no longer has anything to do
    // with window visibility: `Session::run()` blocks on exactly one
    // activation before its first `connect()` and never asks again, so
    // gating that on a show/hotkey gesture only bought a coupling nobody
    // wanted once the window became independently movable/minimizable/
    // not-always-on-top.
    let (ui_rx, ui_tx) = activation_desktop::build_activation_channel();
    let (session_activation, session_tx) = ChannelActivation::new();
    let _ = session_tx.try_send(ActivationEvent::Show);

    // Built here rather than inside the session task: `.manage()` happens on
    // the builder below, which runs long before `build_session` resolves.
    let session_control = SessionControl::new();
    // Start muted: speaking to the assistant is a thing the user does, not a
    // thing that begins because the app launched. `SessionControl::new()`
    // stays capture-enabled -- that is a `uia-core` contract its headless
    // consumers rely on -- so this is an app-level product decision made
    // explicitly here rather than a change to what the core defaults to.
    //
    // The HUD needs no change to match: it calls `get_capture_enabled` on
    // mount rather than assuming a default, so the mic button renders Muted
    // and one click starts things.
    //
    // Note the scope. This gates *forwarding*: frames are still captured and
    // still measured for the level meter, so the OS microphone indicator is
    // on, and the engine still connects at launch. Nothing the user says
    // reaches the engine until they unmute, which is the privacy-relevant
    // half; closing the device and deferring the connection are separate,
    // larger changes.
    session_control.set_capture_enabled(false);

    // Note: no `request_disconnect()` here. The app starts *connected* and
    // muted, which are two different gates and only one of them is about
    // privacy.
    //
    // This line used to exist and read "start cold ... this stops it costing
    // you at launch too". That premise was wrong: a realtime session bills per
    // token, and a muted mic forwards no audio, so an open connection with
    // nobody talking is a socket and nothing else. What starting cold actually
    // bought was a handshake in front of the user's first word -- the mic
    // button called `request_connect` and then waited on a connect it had just
    // asked for.
    //
    // The mute gate above is what keeps the launch honest. Nothing the user
    // says reaches the engine until they unmute; connecting early only means
    // the pipe is ready when they do.
    let managed_control = session_control.clone();

    tauri::Builder::default()
        // MUST be first: on Linux and Windows the OS starts a NEW process for a
        // deep link, with the URL in argv. This callback runs in the ALREADY
        // RUNNING instance and is the only way the code reaches the process
        // holding the PKCE verifier. `on_open_url` alone does not fire there.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // `handle_cli_arguments` ignores the URL unless it is the SOLE CLI
            // argument -- so adding any CLI flag to this app silently breaks
            // deep links. If a flag is ever added, this is what to revisit.
            for arg in argv.iter().skip(1) {
                if oauth_flow::is_callback_url(arg) {
                    oauth_flow::handle_deep_link(app, arg);
                }
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(oauth_flow::DeepLinkListener::default())
        .manage(mcp_registry::PendingOAuthSessions::default())
        .manage(SettingsPath(settings_path))
        .manage(CurrentEngine(engine_choice))
        .manage(FoundrySettingsPath(foundry_settings_path))
        .manage(ConfiguredFoundryEndpoint(configured_foundry_endpoint))
        .manage(AudioSettingsPath(audio_settings_path))
        .manage(CurrentAudioSettings(current_audio_settings))
        .manage(AgentSettingsPath(agent_settings_path))
        .manage(ModelSettingsPath(model_settings_path))
        .manage(current_model_settings)
        .manage(UiSettingsPath(ui_settings_path))
        .manage(SessionControlState(managed_control))
        .manage(McpRegistryPath(mcp_registry_path))
        .manage(McpLocalServersDir(mcp_local_servers_dir.clone()))
        .manage(McpHealthState(mcp_health.clone()))
        .manage(ConversationStorePath(memory_path.clone()))
        .manage(UpdaterState::default())
        .invoke_handler(tauri::generate_handler![
            set_engine,
            get_engine,
            get_secret_status,
            set_secret,
            delete_secret,
            get_foundry_endpoint,
            set_foundry_endpoint,
            get_audio_settings,
            set_audio_settings,
            list_input_devices,
            list_output_devices,
            get_model,
            set_model,
            get_ui_settings,
            set_ui_settings,
            get_agent_settings,
            set_agent_settings,
            get_personas,
            save_personas,
            set_persona_avatar,
            get_active_persona_avatar_path,
            get_start_at_login,
            set_start_at_login,
            set_capture_enabled,
            get_capture_enabled,
            set_connection_wanted,
            get_connection_wanted,
            send_typed_turn,
            get_text_turn_support,
            get_history,
            list_mcp_local_servers,
            install_mcp_local_server,
            remove_mcp_local_server,
            set_mcp_local_server_enabled,
            describe_mcp_local_server,
            get_mcp_local_server_config,
            set_mcp_local_server_config,
            list_mcp_server_health,
            list_mcp_remote_servers,
            preview_mcp_remote_server,
            add_mcp_remote_server,
            set_mcp_remote_server_enabled,
            remove_mcp_remote_server,
            get_update_status,
            check_for_update,
            install_update
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // macOS delivers a deep link through this plugin's own event
            // rather than argv (unlike Linux/Windows, see the
            // `single_instance` comment above): `register()` is
            // `UnsupportedPlatform` there, and a bundled `.app`'s
            // `Info.plist` URL type is what actually invokes this callback,
            // in the same already-running process -- no second instance to
            // forward from.
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let open_url_handle = handle.clone();
                app.deep_link().on_open_url(move |event| {
                    for url in event.urls() {
                        oauth_flow::handle_deep_link(&open_url_handle, url.as_str());
                    }
                });
            }

            // A restart carries already-persisted avatars forward
            // (`set_persona_avatar` only scopes one for the running session
            // that picked it) — re-admit every persona's here so each one
            // renders on every launch, not just the one where it was chosen.
            let personas_path = uia_app::personas::personas_path_for(&config_path);
            for persona in &uia_app::personas::load_personas(&personas_path)
                .book
                .personas
            {
                let Some(ext) = &persona.avatar_ext else {
                    continue;
                };
                let path = settings::persona_avatar_path_for(&config_path, &persona.id, ext);
                let _ = handle.asset_protocol_scope().allow_file(&path);
            }
            app.manage(PersonasPath(personas_path));

            activation_desktop::register(&handle, DEFAULT_ACCELERATOR, ui_tx.clone())?;

            let show_item = MenuItemBuilder::with_id("show", "Show").build(app)?;
            let hide_item = MenuItemBuilder::with_id("hide", "Hide").build(app)?;
            let settings_item = MenuItemBuilder::with_id("settings", "Settings…").build(app)?;
            let update_item =
                MenuItemBuilder::with_id("check-updates", "Check for updates…").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .item(&show_item)
                .item(&hide_item)
                .separator()
                .item(&settings_item)
                .item(&update_item)
                .separator()
                .item(&quit_item)
                .build()?;

            let tray_tx = ui_tx.clone();
            let mut tray = TrayIconBuilder::new()
                .menu(&tray_menu)
                .show_menu_on_left_click(true);
            // `TrayIconBuilder` sets no icon on its own - without an explicit
            // `.icon()` call the tray entry is created with no image at all
            // (confirmed against tauri 2.12.2's source: `build()` passes
            // straight through to the underlying `tray_icon` crate with
            // whatever `.icon()` set, no fallback to the app's window icon).
            // Reuse the icon baked in from `icons/icon.png` via
            // `generate_context!()` so the tray matches the taskbar/window
            // icon instead of showing nothing.
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.on_menu_event(move |app, event| {
                let id = event.id().as_ref();
                if id == "quit" {
                    app.exit(0);
                    return;
                }
                if id == "settings" {
                    // Settings is now an in-window card (PLAN.md Stage 7
                    // decision #2), not a second Tauri window: show/focus the
                    // one `hud` window and let the frontend switch what it
                    // renders in response to the event.
                    if let Some(hud) = app.get_webview_window(HUD_WINDOW) {
                        let _ = hud.show();
                        let _ = hud.set_focus();
                        let _ = hud.emit(OPEN_SETTINGS_EVENT, ());
                    }
                    return;
                }
                if id == "check-updates" {
                    // Opens Settings, where the result is shown: an
                    // "up to date" answer has nowhere else to appear.
                    if let Some(hud) = app.get_webview_window(HUD_WINDOW) {
                        let _ = hud.show();
                        let _ = hud.set_focus();
                        let _ = hud.emit(OPEN_SETTINGS_EVENT, ());
                    }
                    let check_app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        updater::check_now(&check_app, CheckTrigger::Manual).await;
                    });
                    return;
                }
                if let Some(ev) = activation_event_for_menu_id(id) {
                    let _ = tray_tx.try_send(ev);
                }
            })
            .build(app)?;

            // Announce the initial state so a HUD that mounts after this fires
            // still has something to render.
            emit_state(&handle, uia_core::session::State::Idle);
            // Best-effort only, kept for a HUD that happens to already be
            // listening: this can fire before the webview finishes
            // registering its `listen()`, and Tauri does not buffer events
            // for late listeners, so it must never be the only way the HUD
            // learns the engine choice (S21 found exactly that: a restart
            // into Nova rendered "OpenAI" selected because this emit lost
            // the race). `get_engine` is the reliable, request/response path
            // `App.svelte` actually depends on.
            let _ = handle.emit(ENGINE_EVENT, engine_payload(engine_choice));

            // Window lifecycle, decoupled from settings: pin always-on-top
            // from the resolved agent settings, and make a close (native
            // Alt+F4/taskbar-close, or the HUD's own future ✕ button) hide to
            // the tray instead of quitting, unless the user opted into
            // quit-on-close. This is the one place both are wired, so the
            // custom close button (Phase 3) needs no special-case logic of
            // its own — it just calls `window.close()`.
            if let Some(hud) = handle.get_webview_window(HUD_WINDOW) {
                let _ = hud.set_always_on_top(always_on_top);
                let hud_to_hide = hud.clone();
                hud.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event
                        && !quit_on_close
                    {
                        api.prevent_close();
                        let _ = hud_to_hide.hide();
                    }
                });
            }

            // Update checks: 30 s after launch, then every 6 h, each one
            // gated on the agent settings' `auto_update_check`.
            updater::spawn_background_checks(handle.clone(), agent_settings_path_for(&config_path));

            // Mirror the connection gate onto the webview. Its own `watch` is
            // the source of truth; this only republishes it, so the UI can
            // never disagree with what the session is actually doing.
            let conn_handle = handle.clone();
            let mut conn_rx = session_control.subscribe_connection_wanted();
            tauri::async_runtime::spawn(async move {
                loop {
                    let wanted = *conn_rx.borrow_and_update();
                    let _ = conn_handle.emit(CONNECTION_EVENT, connection_payload(wanted));
                    if conn_rx.changed().await.is_err() {
                        break;
                    }
                }
            });

            // Drain the UI activation channel: window show/hide/toggle only.
            // The session no longer listens on this channel at all (see the
            // `session_tx.try_send` above, in `main`) — showing or hiding the
            // overlay must never start or stop the voice session.
            let drain_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut ui_rx = ui_rx;
                while let Some(event) = ui_rx.next().await {
                    if let Some(hud) = drain_handle.get_webview_window(HUD_WINDOW) {
                        let _ = apply_to_window(&hud, event);
                    }
                }
            });

            // The session itself.
            let session_handle = handle.clone();
            // Cloned into the task rather than borrowed: the setup closure
            // returns long before the spawned session future resolves.
            let task_registry = mcp_registry.clone();
            let task_local_servers_dir = mcp_local_servers_dir.clone();
            let task_health = mcp_health.clone();
            tauri::async_runtime::spawn(async move {
                let build = build_session(
                    &config,
                    engine_choice,
                    Box::new(session_activation),
                    &store,
                    &persona_book,
                    agent_name.as_deref(),
                    // The same handle the commands hold. `build_session` sets
                    // it on the session itself, and gives the `switch_persona`
                    // tool a clone of it — the two must not diverge.
                    session_control,
                    memory_path.clone(),
                    &task_registry,
                    &task_local_servers_dir,
                    home_location.as_deref(),
                    &task_health,
                );
                let mut session = match build.await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("uia: cannot start a session: {e}");
                        return;
                    }
                };

                // Subscribed BEFORE `run`, or the first transitions are missed.
                let mut states = session.subscribe_state();
                let mut level = session.subscribe_level();
                let mut errors = session.subscribe_error();
                let mut turns = session.subscribe_turn();

                let state_handle = session_handle.clone();
                tauri::async_runtime::spawn(async move {
                    // A lagged HUD skips to the newest state rather than
                    // dying: falling behind must never stop the status text
                    // updating again.
                    loop {
                        match states.recv().await {
                            Ok(s) => emit_state(&state_handle, s),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });

                let level_handle = session_handle.clone();
                tauri::async_runtime::spawn(async move {
                    let mut ticker = tokio::time::interval(LEVEL_INTERVAL);
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        ticker.tick().await;
                        let rms = *level.borrow_and_update();
                        let _ = level_handle.emit(LEVEL_EVENT, level_payload(rms));
                    }
                });

                let error_handle = session_handle.clone();
                tauri::async_runtime::spawn(async move {
                    // Also on stderr: the terminal is watched during manual
                    // testing (UIA_DUMP_EVENTS lives there too), and a
                    // crashed/never-opened webview must not be the only place
                    // this shows up.
                    loop {
                        match errors.recv().await {
                            Ok(msg) => {
                                eprintln!("uia: engine error: {msg}");
                                let _ = error_handle.emit(ERROR_EVENT, error_payload(&msg));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });

                let turn_handle = session_handle.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match turns.recv().await {
                            Ok(turn) => {
                                let _ =
                                    turn_handle.emit(TURN_EVENT, uia_app::hud::turn_payload(&turn));
                            }
                            // A HUD that fell behind skips to the newest
                            // exchanges rather than dying. It has `get_history`
                            // for the complete record, so dropped events cost
                            // it nothing it cannot recover.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });

                // The capability signal (S0). Subscribed here, next to the
                // other three, but fed by a `watch` rather than a broadcast:
                // this is a state, so a lagging HUD wants the newest value,
                // never a backlog. `publish_text_turn_support` already
                // suppresses no-op sends, so every wake-up here is a real
                // transition — an engine switch, or FD3's degrade.
                let mut support = session.control().subscribe_text_turn_support();
                let support_handle = session_handle.clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        // Emit the value current at subscription time first,
                        // then only on change: the session publishes its
                        // engine's answer during `set_control`, above, which
                        // has already happened by the time this task runs.
                        let payload =
                            uia_app::hud::text_turn_support_payload(&support.borrow_and_update());
                        let _ = support_handle.emit(TEXT_TURN_SUPPORT_EVENT, payload);
                        if support.changed().await.is_err() {
                            break;
                        }
                    }
                });

                let outcome = session.run().await;
                eprintln!("uia: session ended: {outcome:?}");
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error building the uia-app Tauri application")
        .run(|_app, _event| {});
}

fn emit_state(app: &AppHandle, state: uia_core::session::State) {
    app.state::<UpdaterState>().set_session_state(state);
    let _ = app.emit(STATE_EVENT, state_payload(state));
}

/// The running version plus the last update status, for a card that mounts
/// after the last `uia://update` event fired (Tauri does not buffer events).
#[derive(serde::Serialize)]
struct UpdateSnapshot {
    current_version: String,
    status: UpdateStatus,
}

#[tauri::command]
fn get_update_status(app: AppHandle, state: tauri::State<UpdaterState>) -> UpdateSnapshot {
    UpdateSnapshot {
        current_version: app.package_info().version.to_string(),
        status: state.status(),
    }
}

/// The manual check behind Settings' "Check now" and the tray item. Works
/// whether or not automatic checks are on.
#[tauri::command]
async fn check_for_update(app: AppHandle) -> UpdateStatus {
    updater::check_now(&app, CheckTrigger::Manual).await
}

/// Refused (Err) while the assistant is mid-turn; see `updater::may_install`.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    updater::install(&app).await
}

/// Persists the selector's choice for the *next* launch. Restart-to-switch
/// (S18; PLAN.md amended): this does not touch the running session, and
/// `App.svelte` tells the user to restart after calling it. All decision
/// logic (the wire-string parse, the file write) lives in
/// `config::EngineChoice::from_str` and `settings::save`, both tested; this
/// is only the `tauri::command` glue, per this file's own header note.
#[tauri::command]
fn set_engine(state: tauri::State<SettingsPath>, engine: String) -> Result<(), String> {
    let choice = EngineChoice::from_str(&engine)?;
    settings::save(&state.0, &settings::EngineSettings { engine: choice })
        .map_err(|e| e.to_string())
}

/// Returns the engine this launch actually started with, as a request/response
/// call rather than the `ENGINE_EVENT` fired once in `setup()` — see
/// `CurrentEngine`'s doc comment for why the emit alone is race-prone.
/// `App.svelte` calls this on mount so the selector highlights the engine
/// that is actually connected, not whatever the frontend's own default was
/// before any event (if any) arrived.
#[tauri::command]
fn get_engine(state: tauri::State<CurrentEngine>) -> &'static str {
    state.0.as_wire()
}

/// Gates capture *forwarding* on the running session — the mic button's
/// backend (PLAN.md FD6, FD11, FD15). Not activation: the session stays
/// connected and listening either way, and nothing here starts, stops or
/// reconnects it. Infallible and instant by construction — the handle is an
/// atomic flag the capture loop reads once per frame, so there is no channel
/// to be full and no task to be blocked, which is what lets a push-to-talk
/// button call this on every mouse-down and mouse-up.
#[tauri::command]
fn set_capture_enabled(state: tauri::State<SessionControlState>, enabled: bool) {
    state.0.set_capture_enabled(enabled);
    // Unmuting means "I want to talk", and there may be no connection to talk
    // over. Not at launch any more -- the app starts connected -- but Settings
    // drops the session while it is open (`CardDeck.svelte`), and a transport
    // failure can leave it parked. Without this the mic button is dead in
    // exactly those cases: capture ungates, frames go nowhere, nothing
    // happens, and the user has no way to tell why.
    //
    // Deliberately one-way. Muting does NOT disconnect: a mid-conversation
    // mute is a pause, and making it drop the session would turn every pause
    // into a reconnect. Dropping the connection is for Settings and the idle
    // deadline, both of which say something stronger than "quiet for a moment".
    if enabled {
        state.0.request_connect();
    }
}

/// Reads the gate back, for the same reason `get_engine` exists: the webview
/// reloads (dev-server HMR, a settings round trip) and must be able to render
/// the mic button in the state the session is actually in rather than assuming
/// its own default.
#[tauri::command]
fn get_capture_enabled(state: tauri::State<SessionControlState>) -> bool {
    state.0.capture_enabled()
}

/// Ask the running session to hold or drop its engine connection -- the
/// backend for dropping it while Settings is open, and for a user who wants
/// the app resident without a live session.
///
/// Unlike the capture gate this is not free: reconnecting costs a fresh
/// connect. It is for deliberate gestures, not anything per-frame.
#[tauri::command]
fn set_connection_wanted(state: tauri::State<SessionControlState>, wanted: bool) {
    if wanted {
        state.0.request_connect();
    } else {
        state.0.request_disconnect();
    }
}

/// Reads the gate back, for the same reason `get_capture_enabled` exists: the
/// webview reloads and must render the state the session is actually in.
#[tauri::command]
fn get_connection_wanted(state: tauri::State<SessionControlState>) -> bool {
    state.0.connection_wanted()
}

/// Submits a user turn as text rather than audio (PLAN.md S4) — the full
/// HUD's text input's backend. Reaches the running session through the same
/// handle `set_capture_enabled` uses, but as a queued message rather than a
/// flag: unlike the mic gate, this carries a payload and is rare enough
/// (user-typed, not per-frame) to afford a channel hop before the run loop
/// calls the engine. All three engines have a text-input path as of S1
/// (Bedrock's is Nova 2 Sonic's cross-modal text turn, PLAN.md FD7/FD8); a
/// turn that still fails surfaces as an `Err` string from the engine, not
/// from this command, which only reports whether the turn was *queued*.
#[tauri::command]
fn send_typed_turn(state: tauri::State<SessionControlState>, text: String) -> Result<(), String> {
    state.0.submit_text_turn(text).map_err(|e| e.to_string())
}

/// The running session's typed-turn capability (PLAN.md FD2, S0), as the flat
/// payload `hud::text_turn_support_payload` defines — the same shape
/// `TEXT_TURN_SUPPORT_EVENT` carries, so a consumer has one thing to parse
/// rather than two. Request/response for the same reason `get_engine` exists:
/// the webview reloads (HMR, a settings round trip) and must be able to
/// render the control in the state the session is actually in rather than
/// waiting for a change that may never come. S3 is what consumes it.
#[tauri::command]
fn get_text_turn_support(state: tauri::State<SessionControlState>) -> serde_json::Value {
    uia_app::hud::text_turn_support_payload(&state.0.text_turn_support())
}

/// The stored conversation, oldest exchange first, for the history card.
///
/// Read from disk rather than from the running session: the session owns its
/// `FileMemory` by value and never gives it back, and the store is appended on
/// every completed exchange, so loading it here is both possible and current.
/// The cost is that an exchange still in progress is absent until it flushes,
/// which is the right answer for a "conversation so far" view and avoids a
/// second source of truth.
///
/// `enabled` distinguishes the two empty histories: memory turned off, versus
/// memory on with nothing recorded yet. They look identical in `turns` and
/// mean opposite things to a reader.
///
/// Either half of an exchange may be absent. Everything recorded before the
/// session began asking engines to transcribe the user has an assistant half
/// and nothing else, and nothing can backfill it.
#[tauri::command]
fn get_history(state: tauri::State<ConversationStorePath>) -> serde_json::Value {
    let Some(path) = state.0.clone() else {
        return serde_json::json!({ "enabled": false, "turns": [] });
    };
    let turns: Vec<serde_json::Value> = uia_app::memory::FileMemory::load(path)
        .turns()
        .into_iter()
        .map(|t| serde_json::json!({ "user": t.user, "assistant": t.assistant }))
        .collect();
    serde_json::json!({ "enabled": true, "turns": turns })
}

/// Reports whether `account` has a value in the OS keystore - keystore only,
/// not any of the other tiers `resolve_api_key`/`resolve_credentials` fall
/// back to. A credential that resolves from an env var, key file, or inline
/// `uia.toml` value will report as "not configured" here even though it
/// resolves fine for the engine itself: an accurate any-tier check would need
/// the loaded `Config`, which is not `tauri::State`-managed anywhere in this
/// file (only `SettingsPath` and `CurrentEngine` are) and wiring it in is out
/// of scope for this command surface. Never returns the value itself. The
/// settings UI uses this to show "configured" vs "not configured" per field.
#[tauri::command]
fn get_secret_status(account: String) -> bool {
    uia_app::secrets::validate_account(&account).is_ok()
        && uia_app::secrets::OsKeyring.get(&account).is_some()
}

/// Writes directly to the OS keystore. The frontend never reads this value
/// back - write-only, matching SP1's rule that the webview never sees
/// credentials.
#[tauri::command]
fn set_secret(account: String, value: String) -> Result<(), String> {
    uia_app::secrets::validate_account(&account).map_err(|e| e.to_string())?;
    uia_app::secrets::OsKeyring
        .set(&account, &value)
        .map_err(|e| e.to_string())
}

/// Removes `account` from the OS keystore.
#[tauri::command]
fn delete_secret(account: String) -> Result<(), String> {
    uia_app::secrets::validate_account(&account).map_err(|e| e.to_string())?;
    uia_app::secrets::OsKeyring
        .delete(&account)
        .map_err(|e| e.to_string())
}

/// Not a secret (see `FoundrySection::endpoint`'s doc comment), so unlike the
/// three commands above this returns the actual value rather than a
/// configured/not-configured bool - the settings UI shows it in a plain text
/// field, not a password one.
#[tauri::command]
fn get_foundry_endpoint(
    state: tauri::State<FoundrySettingsPath>,
    configured: tauri::State<ConfiguredFoundryEndpoint>,
) -> Option<String> {
    settings::resolve_foundry_endpoint(configured.0.clone(), settings::load(&state.0))
}

/// Writes the sidecar file `get_foundry_endpoint` reads from - never touches
/// `uia.toml` itself, since `Config` is deliberately load-only (see this
/// module's `settings::save` doc comment). An empty/whitespace-only value
/// clears the sidecar override rather than persisting an empty string, so
/// `resolve_foundry_endpoint` falls back to `uia.toml`'s inline value
/// again.
#[tauri::command]
fn set_foundry_endpoint(
    state: tauri::State<FoundrySettingsPath>,
    endpoint: String,
) -> Result<(), String> {
    let trimmed = endpoint.trim();
    let endpoint = if trimmed.is_empty() {
        None
    } else {
        // Checked server-side too, not just in Settings.svelte's own check -
        // the frontend's is UX (instant feedback, no round trip), this one is
        // the actual guarantee that `uia-foundry.json` never ends up
        // holding something `resolve_foundry_endpoint` would happily hand to
        // an HTTP client as a base URL.
        if !settings::is_valid_http_url(trimmed) {
            return Err(format!(
                "{trimmed:?} is not a valid http:// or https:// URL"
            ));
        }
        Some(trimmed.to_string())
    };
    settings::save(&state.0, &settings::FoundrySettings { endpoint }).map_err(|e| e.to_string())
}

/// The Providers tab's initial model values (FD3) - the same values
/// `build_session` already started the session with, not re-derived here
/// from `uia.toml` plus the sidecar separately. `provider` is `"openai"`,
/// `"bedrock"` or `"foundry"` - one arm per engine, including Bedrock,
/// whose id used to be a hardcoded static label with nothing to return.
#[tauri::command]
fn get_model(
    state: tauri::State<CurrentModelSettings>,
    provider: String,
) -> Result<String, String> {
    match provider.as_str() {
        "openai" => Ok(state.openai.clone()),
        "bedrock" => Ok(state.bedrock.clone()),
        "foundry" => Ok(state.foundry.clone()),
        other => Err(format!("unknown model provider {other:?}")),
    }
}

/// Writes the sidecar file `get_model` reads from at the *next* startup -
/// same restart-to-switch rule as the engine choice (S18) and FD5: a
/// running session already baked its model choice into `build_session`, and
/// this command does not reach into it live.
#[tauri::command]
fn set_model(
    state: tauri::State<ModelSettingsPath>,
    provider: String,
    value: String,
) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("model value must not be empty".to_string());
    }
    let mut settings: settings::ModelSettings = settings::load(&state.0).unwrap_or_default();
    match provider.as_str() {
        "openai" => settings.openai = Some(trimmed.to_string()),
        "bedrock" => settings.bedrock = Some(trimmed.to_string()),
        "foundry" => settings.foundry = Some(trimmed.to_string()),
        other => return Err(format!("unknown model provider {other:?}")),
    }
    settings::save(&state.0, &settings).map_err(|e| e.to_string())
}

/// The Visual tab's persisted values, returned raw - no
/// `prefers-color-scheme` fallback logic here (the frontend supplies that
/// when `theme` is absent, see `settings::UiSettings`'s doc comment). Unlike
/// `get_audio_settings`/`get_model`, there is no `uia.toml` counterpart
/// to resolve against, so this reads the sidecar directly rather than a
/// `tauri::State` snapshot from startup - nothing else needs a "state at
/// launch" for a tab that always applies live (FD5).
#[tauri::command]
fn get_ui_settings(state: tauri::State<UiSettingsPath>) -> settings::UiSettings {
    settings::load(&state.0).unwrap_or_default()
}

/// Writes the sidecar file `get_ui_settings` reads from. Applies live (FD5)
/// - no restart note here, unlike `set_model`/`set_engine`.
#[tauri::command]
fn set_ui_settings(
    state: tauri::State<UiSettingsPath>,
    settings: settings::UiSettings,
) -> Result<(), String> {
    settings::save(&state.0, &settings).map_err(|e| e.to_string())
}

/// The General tab's assistant-identity and app-lifecycle fields (FD9) —
/// read directly from the sidecar rather than a `tauri::State` snapshot,
/// same as `get_ui_settings`, since `always_on_top`/`quit_on_close` are the
/// only fields of this file `main()` also consults, and both of those are
/// restart-to-switch already covered by `CurrentEngine`-style state being
/// unnecessary for a settings *tab* whose job is showing what's on disk.
#[tauri::command]
fn get_agent_settings(state: tauri::State<AgentSettingsPath>) -> settings::AgentSettings {
    settings::load(&state.0).unwrap_or_default()
}

/// Writes the sidecar file `get_agent_settings` reads from. `name` is an FD5
/// restart-required field (baked into `build_session`'s system prompt);
/// `always_on_top`/`quit_on_close` are also resolved once at startup (see
/// `main`), so nothing here applies live — same restart note as `set_model`.
#[tauri::command]
fn set_agent_settings(
    state: tauri::State<AgentSettingsPath>,
    settings: settings::AgentSettings,
) -> Result<(), String> {
    settings::save(&state.0, &settings).map_err(|e| e.to_string())
}

/// The Personas frame's initial state: the book plus anything `load_personas`
/// had to correct on the way in, so the UI can show why a persona it wrote
/// came back disabled.
#[tauri::command]
fn get_personas(state: tauri::State<PersonasPath>) -> serde_json::Value {
    let outcome = uia_app::personas::load_personas(&state.0);
    let avatars = uia_app::personas::avatar_paths(&state.0, &outcome.book);
    serde_json::json!({
        "book": outcome.book,
        "notices": outcome.notices,
        "avatar_paths": avatars,
    })
}

/// Writes the book the frontend edited. Runs it through `LoadOutcome` first
/// so the six-persona cap and the enabled-requires-complete rule are enforced
/// on the way *in* as well as the way out — a frontend bug must not be able
/// to write a book the loader would then have to repair.
#[tauri::command]
fn save_personas(
    state: tauri::State<PersonasPath>,
    book: uia_app::personas::PersonaBook,
) -> Result<serde_json::Value, String> {
    let outcome = uia_app::personas::LoadOutcome::from_book(book);
    uia_app::personas::save_personas(&state.0, &outcome.book).map_err(|e| e.to_string())?;
    let avatars = uia_app::personas::avatar_paths(&state.0, &outcome.book);
    Ok(serde_json::json!({
        "book": outcome.book,
        "notices": outcome.notices,
        "avatar_paths": avatars,
    }))
}

/// The HUD's read-only avatar preview, now sourced from the active persona
/// rather than the single global avatar `set_agent_avatar` used to own.
/// `None` when the active persona has no avatar — the HUD already treats a
/// `null` path as "no avatar" (`HudCard.svelte`'s `no-avatar` column).
#[tauri::command]
fn get_active_persona_avatar_path(state: tauri::State<PersonasPath>) -> Option<String> {
    let book = uia_app::personas::load_personas(&state.0).book;
    let persona = book.personas.iter().find(|p| p.id == book.active)?;
    let ext = persona.avatar_ext.as_ref()?;
    let path = settings::persona_avatar_path_for(&state.0, &persona.id, ext);
    Some(path.to_string_lossy().to_string())
}

/// Copies the chosen image to this persona's avatar file and admits it to the
/// asset protocol scope, exactly as `set_agent_avatar` did for the single
/// persona — scoped to the one file, never the config directory, since
/// `uia.toml`'s sibling `*.key` credential files must never become reachable
/// through `asset://`.
#[tauri::command]
fn set_persona_avatar(
    app: tauri::AppHandle,
    settings_state: tauri::State<AgentSettingsPath>,
    personas_state: tauri::State<PersonasPath>,
    id: String,
    source_path: String,
) -> Result<String, String> {
    let dest =
        settings::copy_agent_avatar(&settings_state.0, &id, std::path::Path::new(&source_path))
            .map_err(|e| e.to_string())?;
    // Copying the image is not choosing it. `avatar_ext` is the only thing
    // that says a persona has an avatar at all, so without this the next
    // `load_personas` reads `None` and the face never appears, with the file
    // sitting on disk the whole time. `copy_agent_avatar` has already
    // lowercased and validated the extension, so it is taken from the
    // destination rather than re-derived from the source.
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| "the copied avatar has no extension".to_string())?;
    uia_app::personas::set_avatar_ext(&personas_state.0, &id, ext).map_err(|e| e.to_string())?;
    app.asset_protocol_scope()
        .allow_file(&dest)
        .map_err(|e| e.to_string())?;
    Ok(dest.to_string_lossy().to_string())
}

/// Whether the app is currently registered to launch at OS login —
/// `tauri-plugin-autostart`'s own `enable()`/`disable()`/`is_enabled()` is
/// the single source of truth (deliberately not mirrored into a sidecar
/// boolean, so this can never disagree with what the OS actually has
/// registered).
#[tauri::command]
fn get_start_at_login(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Enables/disables OS-login autostart via the plugin directly.
#[tauri::command]
fn set_start_at_login(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autostart = app.autolaunch();
    if enabled {
        autostart.enable().map_err(|e| e.to_string())
    } else {
        autostart.disable().map_err(|e| e.to_string())
    }
}

/// The Audio tab's initial values - the same `AudioSection` `build_session`
/// already opened devices with (see `main`'s `resolve_audio_settings` call),
/// not re-derived here from `uia.toml` plus the sidecar separately, so
/// this can never disagree with what the running session actually did.
#[tauri::command]
fn get_audio_settings(state: tauri::State<CurrentAudioSettings>) -> AudioSection {
    state.0.clone()
}

/// Writes the sidecar file `settings::resolve_audio_settings` reads at the
/// *next* startup - same restart-to-switch rule as the engine choice
/// (S18): a running session already opened its devices and set its
/// barge-in threshold, and this command does not reach into it live.
/// `input_device`/`output_device` of `None` means "use whatever the OS
/// calls default" (the picker's "Default" option), not "no change" - the
/// frontend always sends its full current selection.
#[tauri::command]
fn set_audio_settings(
    state: tauri::State<AudioSettingsPath>,
    input_device: Option<String>,
    output_device: Option<String>,
    aec_enabled: bool,
    barge_in_threshold: f32,
) -> Result<(), String> {
    settings::save(
        &state.0,
        &settings::AudioSettings {
            input_device,
            output_device,
            aec_enabled: Some(aec_enabled),
            barge_in_threshold: Some(barge_in_threshold),
        },
    )
    .map_err(|e| e.to_string())
}

/// Every input device name the UI and the startup filter see, from one place
/// per OS. On macOS this is CoreAudio's own list rather than cpal's: the app
/// runs a Voice Processing IO unit, and while it runs cpal lists every
/// speaker as an input too (the unit's echo-reference taps) plus the unit's
/// private aggregate. Each name appears once on every OS.
fn input_device_names() -> Result<Vec<String>, uia_core::audio::AudioError> {
    #[cfg(target_os = "macos")]
    {
        uia_audio::coreaudio::list_input_device_names()
    }
    #[cfg(not(target_os = "macos"))]
    {
        uia_audio::input::list_input_devices()
    }
}

/// Output counterpart of [`input_device_names`].
fn output_device_names() -> Result<Vec<String>, uia_core::audio::AudioError> {
    #[cfg(target_os = "macos")]
    {
        uia_audio::coreaudio::list_output_device_names()
    }
    #[cfg(not(target_os = "macos"))]
    {
        uia_audio::output::list_output_devices()
    }
}

/// Names for the Audio tab's input-device dropdown, via
/// [`input_device_names`]. On macOS they come from CoreAudio's
/// `kAudioDevicePropertyDeviceNameCFString`, the same name property cpal
/// reports and `uia_audio::coreaudio` resolves `audio.input_device` against.
/// Elsewhere they are cpal's list - see `uia_audio::input::list_input_devices`'s
/// doc comment. Either way a name picked here resolves when opening.
#[tauri::command]
fn list_input_devices() -> Result<Vec<String>, String> {
    input_device_names().map_err(|e| e.to_string())
}

/// Names for the Audio tab's output-device dropdown.
#[tauri::command]
fn list_output_devices() -> Result<Vec<String>, String> {
    output_device_names().map_err(|e| e.to_string())
}

/// The `.mcpb` local servers this install has approved. The stored record only —
/// the tool list the model receives is always re-queried live at session
/// start (`session::build_executor`), never read from here.
#[tauri::command]
fn list_mcp_local_servers(
    state: tauri::State<McpRegistryPath>,
) -> Vec<mcp_registry::LocalServerEntry> {
    mcp_registry::load(&state.0).local_servers
}

/// Validates and extracts a `.mcpb` the frontend's file dialog resolved,
/// then records it — disabled, per `add_local_server`. Every rejection reason is
/// a `BundleError` whose `Display` is written for the user, so it is
/// returned verbatim for the tab to show inline.
///
/// Once `install_bundle` has extracted the files, every remaining failure —
/// `add_local_server` rejecting a duplicate name, or `save` itself failing (a
/// read-only config dir, disk full) — is funneled through one cleanup that
/// deletes `installed.dir`. Either failure alone would otherwise leave an
/// unlisted directory nobody can remove from the UI: `list_mcp_local_servers`
/// would not show it, `remove_mcp_local_server` would say `NoSuchLocalServer`, and a
/// retry would hit `BundleError::AlreadyInstalled`.
#[tauri::command]
fn install_mcp_local_server(
    registry_state: tauri::State<McpRegistryPath>,
    local_servers_state: tauri::State<McpLocalServersDir>,
    source_path: String,
) -> Result<mcp_registry::LocalServerEntry, String> {
    let installed =
        uia_mcp::install_bundle(std::path::Path::new(&source_path), &local_servers_state.0)
            .map_err(|e| e.to_string())?;

    let mut registry = mcp_registry::load(&registry_state.0);
    let record = registry
        .add_local_server(installed.name.clone(), installed.version.clone())
        .and_then(|()| mcp_registry::save(&registry_state.0, &registry))
        .map_err(|e| e.to_string())
        .and_then(|()| {
            // The invariant holds — `add_local_server` just pushed this entry — but
            // an IPC handler is the wrong place to prove it with a panic: a
            // violated invariant should surface as a command error the UI can
            // show, not as a crash.
            registry
                .local_servers
                .iter()
                .find(|p| p.name == installed.name)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "local server {:?} was added to the registry but is missing from it",
                        installed.name
                    )
                })
        });

    match record {
        Ok(entry) => Ok(entry),
        Err(e) => {
            std::fs::remove_dir_all(&installed.dir).ok();
            Err(e)
        }
    }
}

/// Drops the record, the extracted files AND the server's saved-settings
/// keyring entries. The registry owns only the record (see
/// `McpRegistry::remove_local_server`), so the directory and the keyring are
/// `mcp_registry::remove_local_server_and_clear_config`'s responsibility.
#[tauri::command]
fn remove_mcp_local_server(
    registry_state: tauri::State<McpRegistryPath>,
    local_servers_state: tauri::State<McpLocalServersDir>,
    name: String,
) -> Result<(), String> {
    mcp_registry::remove_local_server_and_clear_config(
        &registry_state.0,
        &local_servers_state.0,
        &uia_app::secrets::OsKeyring,
        &name,
    )
}

/// Restart-to-apply, like every other MCP change: `build_executor` read the
/// registry once at startup.
#[tauri::command]
fn set_mcp_local_server_enabled(
    state: tauri::State<McpRegistryPath>,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    let mut registry = mcp_registry::load(&state.0);
    registry
        .set_local_server_enabled(&name, enabled)
        .map_err(|e| e.to_string())?;
    mcp_registry::save(&state.0, &registry).map_err(|e| e.to_string())
}

/// Adapter over `mcp_registry::describe_local_server`. The logic lives in the
/// lib because this binary is behind `required-features = ["desktop"]`, so
/// nothing written here is reachable by `cargo test --workspace`.
#[tauri::command]
fn describe_mcp_local_server(
    registry_state: tauri::State<McpRegistryPath>,
    local_servers_state: tauri::State<McpLocalServersDir>,
    name: String,
) -> Result<mcp_registry::LocalServerDetails, String> {
    let registry = mcp_registry::load(&registry_state.0);
    mcp_registry::describe_local_server(&registry, &local_servers_state.0, &name)
}

/// Adapter over `mcp_registry::describe_local_config`. Sensitive values are
/// never in the return value — only whether one is stored.
#[tauri::command]
fn get_mcp_local_server_config(
    registry_state: tauri::State<McpRegistryPath>,
    local_servers_state: tauri::State<McpLocalServersDir>,
    name: String,
) -> Result<Vec<mcp_registry::ConfigFieldView>, String> {
    let registry = mcp_registry::load(&registry_state.0);
    mcp_registry::describe_local_config(
        &registry,
        &local_servers_state.0,
        &uia_app::secrets::OsKeyring,
        &name,
    )
}

/// Restart-to-apply, like every other MCP change. A key left out of `values`
/// is unchanged; `null` (or an empty string) clears it.
#[tauri::command]
fn set_mcp_local_server_config(
    registry_state: tauri::State<McpRegistryPath>,
    local_servers_state: tauri::State<McpLocalServersDir>,
    name: String,
    values: std::collections::BTreeMap<String, Option<String>>,
) -> Result<(), String> {
    mcp_registry::save_local_config(
        &registry_state.0,
        &local_servers_state.0,
        &uia_app::secrets::OsKeyring,
        &name,
        values,
    )
}

/// What each server is contributing to the running session.
///
/// Keyed by server name, and absence is meaningful: a name missing from this
/// map was never attempted by the session that is running, either because it
/// was added or enabled since startup or because no session has started. The
/// panel renders that as "pending restart", never as healthy.
///
/// Read when the panel opens. Its contents move underneath it as the session
/// reconnects, so what a long-open panel shows is what was true when it last
/// read — see the Status section of `docs/MCP.md`.
#[tauri::command]
fn list_mcp_server_health(
    state: tauri::State<McpHealthState>,
) -> std::collections::BTreeMap<String, uia_app::session::ServerHealth> {
    match state.0.lock() {
        Ok(map) => map.clone(),
        // A poisoned lock means a panic elsewhere already happened; reporting
        // "nothing is known" degrades the panel to pending-restart rather
        // than taking the command down with it.
        Err(_) => Default::default(),
    }
}

/// What the frontend is allowed to see about a remote server.
///
/// `header_value` is deliberately absent and `has_header` stands in for it:
/// the value is a bearer token, and echoing it back into the WebView on
/// every tab open would put it somewhere it never needs to be.
#[derive(serde::Serialize)]
struct RemoteServerSummary {
    name: String,
    url: String,
    header_name: Option<String>,
    has_header: bool,
    enabled: bool,
    declared_name: Option<String>,
    declared_version: Option<String>,
    tools: Vec<mcp_registry::DeclaredTool>,
}

impl From<mcp_registry::RemoteEntry> for RemoteServerSummary {
    fn from(e: mcp_registry::RemoteEntry) -> Self {
        Self {
            name: e.name,
            url: e.url,
            has_header: e.header_value.is_some(),
            header_name: e.header_name,
            enabled: e.enabled,
            declared_name: e.declared_name,
            declared_version: e.declared_version,
            tools: e.tools,
        }
    }
}

#[tauri::command]
fn list_mcp_remote_servers(state: tauri::State<McpRegistryPath>) -> Vec<RemoteServerSummary> {
    mcp_registry::load(&state.0)
        .remote_servers
        .into_iter()
        .map(RemoteServerSummary::from)
        .collect()
}

/// What `preview_mcp_remote_server`/`add_mcp_remote_server` hand back to the
/// frontend: either a server that connected, or a browser URL the user must
/// open before either command can succeed. Serialized with a `kind` tag so
/// the frontend can switch on it directly rather than parsing an error
/// string for a URL.
///
/// Wiring the frontend to actually open `authorize_url` and react to the
/// eventual sign-in is Task 8's job — this only guarantees the backend has
/// somewhere honest to put the URL rather than smuggling it through an
/// `Err(String)`.
#[derive(serde::Serialize)]
#[serde(tag = "kind")]
enum PreviewOutcome {
    Connected(RemoteServerSummary),
    NeedsAuthorization { authorize_url: String },
}

/// Connect to a remote server and report what it declares itself to be.
/// Persists nothing — this is the review step, and `add_mcp_remote_server`
/// is the decision.
///
/// An `Ok(PreviewOutcome::NeedsAuthorization { .. })` means the server
/// answered 401: `mcp_registry::preview_remote`'s OAuth branch has already
/// stashed the in-flight `AuthorizationSession` in `PendingOAuthSessions`
/// and this command has spawned `complete_oauth_sign_in` to wait for the
/// browser round trip in the background — see that function's doc comment
/// for what happens next. A caller retries this same command once signed in;
/// see `mcp_registry::preview_remote`'s own doc comment for why that retry
/// needs no dedicated "finish" command.
#[tauri::command]
async fn preview_mcp_remote_server(
    app: tauri::AppHandle,
    name: String,
    url: String,
    header_name: Option<String>,
    header_value: Option<String>,
    oauth_client_id: Option<String>,
    auth: mcp_registry::RemoteAuth,
) -> Result<PreviewOutcome, String> {
    let name_for_task = name.clone();
    let outcome = {
        let pending_sessions = app.state::<mcp_registry::PendingOAuthSessions>();
        let pending_authorizations = app
            .state::<oauth_flow::DeepLinkListener>()
            .pending_authorizations();
        mcp_registry::preview_remote(
            name,
            url,
            header_name,
            header_value,
            oauth_client_id,
            auth,
            pending_sessions.inner(),
            &pending_authorizations,
        )
        .await
    };
    match outcome {
        Ok(entry) => Ok(PreviewOutcome::Connected(RemoteServerSummary::from(entry))),
        Err(mcp_registry::RegistryError::NeedsAuthorization {
            authorize_url,
            state,
        }) => {
            // A repeat call (the frontend's "check again" button, clicked
            // before the human finishes signing in) resumes the SAME
            // pending session now (`preview_remote`'s dedup, finding 2) --
            // so `state` here may be one `complete_oauth_sign_in` already
            // has a background task waiting on. That function checks for
            // exactly this before spawning a second one.
            complete_oauth_sign_in(app, name_for_task, state);
            Ok(PreviewOutcome::NeedsAuthorization { authorize_url })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Waits for the browser round trip a `NeedsAuthorization` preview started,
/// then finishes the token exchange — the second half of the two-phase OAuth
/// flow `mcp_registry::preview_remote` begins.
///
/// Runs to completion in the background rather than being awaited by the
/// Tauri command that spawns it: that command has already returned the
/// authorize URL to the frontend, and nothing here has a caller left to
/// report back to (Task 8's frontend work owns notifying the user once this
/// finishes; this function's only job is making sure the credentials
/// actually land in the keychain).
///
/// `session.handle_callback_url` is what persists the exchanged token into
/// the keychain, via the same `KeyringCredentialStore` clone
/// `mcp_registry::preview_remote` wired into the manager before this session
/// was ever built — nothing further needs to be written here for a
/// subsequent `preview_mcp_remote_server`/`add_mcp_remote_server` retry to
/// find it.
///
/// Idempotent per `state`, guarded by `PendingOAuthSessions::should_spawn_
/// task` (finding 2, 2026-08-30 whole-branch review): since a repeat
/// `preview_mcp_remote_server` call now RESUMES an existing pending session
/// rather than minting a fresh one, it also calls this function again with
/// the SAME `state`. Spawning a second background task for that state would
/// register a second `oneshot` waiter under it in `DeepLinkListener`,
/// silently replacing (and thereby cancelling) the first task's wait rather
/// than running alongside it — so every exit path below releases the guard
/// via `finish_task` once it is actually done with `state`, and a call that
/// finds the guard already held returns immediately, doing nothing.
fn complete_oauth_sign_in(app: tauri::AppHandle, name: String, state: String) {
    use uia_mcp::callback::CallbackListener;

    if !app
        .state::<mcp_registry::PendingOAuthSessions>()
        .should_spawn_task(&state)
    {
        return;
    }

    tauri::async_runtime::spawn(async move {
        let listener = app.state::<oauth_flow::DeepLinkListener>().inner().clone();
        let callback_url = match tokio::time::timeout(
            OAUTH_BROWSER_TIMEOUT,
            listener.await_callback(&state),
        )
        .await
        {
            Ok(Ok(url)) => url,
            Ok(Err(e)) => {
                eprintln!("uia: OAuth sign-in did not complete: {e}");
                let _ = app.emit(
                    OAUTH_SIGN_IN_ERROR_EVENT,
                    oauth_sign_in_error_payload(&name, &e.to_string()),
                );
                app.state::<mcp_registry::PendingOAuthSessions>()
                    .finish_task(&state);
                return;
            }
            Err(_elapsed) => {
                eprintln!("uia: OAuth sign-in timed out waiting for the browser");
                let _ = app.emit(
                    OAUTH_SIGN_IN_ERROR_EVENT,
                    oauth_sign_in_error_payload(
                        &name,
                        "the sign-in page timed out waiting for you to finish",
                    ),
                );
                app.state::<mcp_registry::PendingOAuthSessions>()
                    .finish_task(&state);
                return;
            }
        };

        let pending_sessions = app.state::<mcp_registry::PendingOAuthSessions>();
        let session = pending_sessions.take(&state);
        pending_sessions.finish_task(&state);
        let Some(session) = session else {
            // Already completed (or timed out and was never retried) by
            // another callback for the same state; nothing left to do.
            eprintln!("uia: OAuth callback arrived for an unknown or already-completed sign-in");
            return;
        };

        if let Err(e) = session.handle_callback_url(&callback_url).await {
            eprintln!("uia: OAuth token exchange failed: {e}");
            let _ = app.emit(
                OAUTH_SIGN_IN_ERROR_EVENT,
                oauth_sign_in_error_payload(&name, &e.to_string()),
            );
        }
    });
}

/// Re-runs the declaration and, only if it succeeds, stores the server —
/// disabled, per `add_remote`.
///
/// The connection is made again rather than trusting a preview the frontend
/// hands back: nothing is cached between the two calls, and a backend that
/// stored whatever the WebView claimed a server had said would make the
/// declaration requirement decorative.
///
/// An OAuth server whose sign-in has not completed yet cannot be added:
/// `preview_remote` returns `NeedsAuthorization` rather than an entry to
/// store, so this returns `PreviewOutcome::NeedsAuthorization` the same way
/// `preview_mcp_remote_server` does — a background task is spawned to finish
/// the token exchange, and the caller retries this same command (with the
/// same `RemoteAuth::OAuth`/`oauth_client_id`) once signed in, which is
/// deliberate per that function's own doc comment: it must not start a
/// second browser flow, and a plain retry finds the credentials the first
/// leg already stored.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn add_mcp_remote_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, McpRegistryPath>,
    name: String,
    url: String,
    header_name: Option<String>,
    header_value: Option<String>,
    oauth_client_id: Option<String>,
    auth: mcp_registry::RemoteAuth,
) -> Result<PreviewOutcome, String> {
    let name_for_task = name.clone();
    let outcome = {
        let pending_sessions = app.state::<mcp_registry::PendingOAuthSessions>();
        let pending_authorizations = app
            .state::<oauth_flow::DeepLinkListener>()
            .pending_authorizations();
        mcp_registry::preview_remote(
            name,
            url,
            header_name,
            header_value,
            oauth_client_id,
            auth,
            pending_sessions.inner(),
            &pending_authorizations,
        )
        .await
    };
    let entry = match outcome {
        Ok(entry) => entry,
        Err(mcp_registry::RegistryError::NeedsAuthorization {
            authorize_url,
            state: oauth_state,
        }) => {
            // See `preview_mcp_remote_server`'s matching comment: a repeat
            // call may resume the same pending session, and
            // `complete_oauth_sign_in` is guarded against spawning twice
            // for it.
            complete_oauth_sign_in(app, name_for_task, oauth_state);
            return Ok(PreviewOutcome::NeedsAuthorization { authorize_url });
        }
        Err(e) => return Err(e.to_string()),
    };

    let mut registry = mcp_registry::load(&state.0);
    // An OAuth entry can reach `Connected` here without ever going through
    // `NeedsAuthorization` — its previously-issued access token can still be
    // valid (the orchestrator verifies the JWT locally, not against
    // Cognito's own revocation state), so a "session expired" reconnect
    // attempt on an already-added, already-enabled name looks identical to a
    // fresh success. The frontend's `sessionExpired`/`reconnectedNoAdd`
    // tracking only ever sees the `NeedsAuthorization`-retry shape of that,
    // so it renders "Confirm & add" here regardless — which would otherwise
    // hit `add_remote`'s unconditional `DuplicateName` refusal on a plain
    // "already signed in" reconnect. Recognized here instead: nothing about
    // an existing OAuth entry needs re-declaring just because its credential
    // was refreshed.
    let already_registered = entry.auth == mcp_registry::RemoteAuth::OAuth
        && registry.remote_servers.iter().any(|r| r.name == entry.name);
    if !already_registered {
        registry
            .add_remote(entry.clone())
            .map_err(|e| e.to_string())?;
        mcp_registry::save(&state.0, &registry).map_err(|e| e.to_string())?;
    }

    Ok(PreviewOutcome::Connected(RemoteServerSummary::from(entry)))
}

#[tauri::command]
fn set_mcp_remote_server_enabled(
    state: tauri::State<McpRegistryPath>,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    let mut registry = mcp_registry::load(&state.0);
    registry
        .set_remote_enabled(&name, enabled)
        .map_err(|e| e.to_string())?;
    mcp_registry::save(&state.0, &registry).map_err(|e| e.to_string())
}

#[tauri::command]
/// Adapter over `mcp_registry::remove_remote_and_clear_credentials`. Supplies
/// the real keystore; the logic and its tests live in the lib, which is the
/// only half `cargo test --workspace` can reach.
async fn remove_mcp_remote_server(
    state: tauri::State<'_, McpRegistryPath>,
    name: String,
) -> Result<(), String> {
    let secrets: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(uia_app::secrets::OsKeyring);
    mcp_registry::remove_remote_and_clear_credentials(&state.0, &name, secrets).await
}

/// Applies an [`ActivationEvent`] to the HUD window. The decision itself lives
/// in `hud::window_action_for`, which is tested; this is only the `tauri` call.
fn apply_to_window(window: &tauri::WebviewWindow, event: ActivationEvent) -> tauri::Result<()> {
    match window_action_for(event) {
        WindowAction::Show => window.show(),
        WindowAction::Hide => window.hide(),
        WindowAction::Toggle => {
            if window.is_visible().unwrap_or(false) {
                window.hide()
            } else {
                window.show()
            }
        }
        WindowAction::None => Ok(()),
    }
}

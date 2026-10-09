// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Composition root: turn a `Config` into a runnable `Session`.
//!
//! This is the one place that knows about every crate at once, which is exactly
//! why it is the only place allowed to. `uia-core` still has no idea cpal,
//! WebRTC, Bedrock, or MCP exist.

use crate::config::{Config, EngineChoice};
use crate::mcp_registry::McpRegistry;
use std::sync::Arc;
use uia_core::activation::Activation;
use uia_core::audio::{AudioSink, AudioSource};
use uia_core::engine::S2sEngine;
use uia_core::memory::{MemoryContext, NullMemory};
use uia_core::session::{Session, SessionDeps};
use uia_core::tools::ToolExecutor;
use uia_mcp::{McpExecutor, McpRouter, McpServerConfig};

/// What `name` (the General tab's identity field) falls back to when unset —
/// the one place that default lives, so nothing else needs to know it.
pub const DEFAULT_ASSISTANT_NAME: &str = "MyMy";

/// The FD16 brevity instruction. It is an operational directive about
/// response *length*, not an identity statement, so `compose_prompt` appends
/// it to every prompt it builds regardless of which persona is active —
/// without this, every user who writes their own persona silently loses it.
///
/// Kept to one sentence on purpose: it rides on every session's setup prompt,
/// so its own token cost is paid on every connect. S6 measures against this
/// exact wording — change it there and the comparison stops meaning anything.
pub const BREVITY_INSTRUCTION: &str =
    "Keep replies to one or two short sentences unless the user explicitly asks for more detail.";

/// Stops the assistant opening with an unprompted self-introduction.
///
/// Left to itself it presents itself — announcing its name and what it can
/// do before being asked anything. On a voice HUD that is the first thing
/// heard on every connect, and it also lands in the conversation store as an
/// exchange with an assistant half and no user half, because nobody said
/// anything to prompt it.
///
/// Appended to every prompt for the same reason as [`BREVITY_INSTRUCTION`]:
/// a user persona replaces the default identity body wholesale, and this
/// must not be a directive that only users without a persona get.
///
/// Prompt-level, so it is a strong bias rather than a guarantee — no engine
/// offers a switch for "do not speak first". The greeting-shaped turn is
/// therefore made unlikely here, not impossible, and anything reading the
/// store still has to tolerate a one-sided exchange.
pub const NO_SELF_INTRODUCTION: &str =
    "Never open with a greeting or introduce yourself unless the user greets you first.";

/// The system prompt for whichever persona is active.
///
/// `engine_choice` survives only for the self-identification clause (S21's
/// debugging aid); identity itself comes entirely from the persona, so
/// there is no longer a per-engine default prompt to fall back to — a
/// persona always exists, because `load_personas` guarantees one.
pub fn system_prompt_for(
    engine_choice: EngineChoice,
    persona: &crate::personas::Persona,
    global_name: Option<&str>,
    allow_agent_switch: bool,
) -> String {
    let engine_name = match engine_choice {
        EngineChoice::OpenAi => "OpenAI Realtime",
        EngineChoice::Bedrock => "AWS Nova Sonic",
        EngineChoice::Foundry => "Microsoft AI Foundry",
    };
    format!(
        "{} If asked which engine or model you are running on, say plainly that you are \
         running on {engine_name}.",
        crate::personas::compose_prompt(persona, global_name, allow_agent_switch)
    )
}

/// The persona a book is currently running, or the built-in default when
/// `active` names one that is not there.
///
/// `load_personas` already guarantees `active` resolves, so the fallback is
/// belt and braces for a book built in a test or handed over by a future
/// command — but "no identity at all" is not an option a voice session can
/// take, so it is a fallback rather than an error.
pub fn active_persona(book: &crate::personas::PersonaBook) -> crate::personas::Persona {
    book.personas
        .iter()
        .find(|p| p.id == book.active)
        .cloned()
        .unwrap_or_else(|| crate::personas::PersonaBook::default().personas.remove(0))
}

/// The system prompt a session starts on: the book's *active* persona, with
/// the book's own switching toggle.
///
/// Split from `build_session` because that function opens real audio devices
/// and can therefore have no offline test, while which persona the app boots
/// as is exactly the thing worth asserting.
pub fn system_prompt_for_book(
    engine_choice: EngineChoice,
    book: &crate::personas::PersonaBook,
    global_name: Option<&str>,
) -> String {
    system_prompt_for(
        engine_choice,
        &active_persona(book),
        global_name,
        book.allow_agent_switch,
    )
}

/// The tool surface a session runs with: the MCP executor, with
/// `switch_persona` layered over it.
///
/// Takes the `SessionControl` the caller will also hand the session. That is
/// the whole reason this is a named function rather than three lines inside
/// `build_session`: the decorator requests a swap on this handle and the run
/// loop consumes it from the session's, so if the two are not clones of one
/// another every switch is answered and then silently dropped.
pub fn persona_tools(
    inner: Arc<dyn ToolExecutor>,
    book: &crate::personas::PersonaBook,
    global_name: Option<&str>,
    control: uia_core::session::SessionControl,
) -> Arc<dyn ToolExecutor> {
    Arc::new(crate::persona_executor::PersonaExecutor::new(
        inner,
        book.clone(),
        global_name.map(str::to_string),
        control,
    ))
}

/// The clock, layered under `persona_tools`. A named function beside it for
/// symmetry rather than necessity: this one has no handle to share, but a
/// reader looking for "where do built-in tools get added" should find both in
/// the same place.
pub fn time_tools(inner: Arc<dyn ToolExecutor>) -> Arc<dyn ToolExecutor> {
    Arc::new(crate::time_executor::TimeExecutor::new(
        inner,
        Arc::new(crate::time::SystemClock),
        Arc::new(crate::time::SystemLocalZone),
    ))
}

/// Hardcoded for now — no settings UI exists yet to choose it (see PLAN.md /
/// LEDGER.md for the dropdown-exposure evaluation). OpenAI-specific: this is
/// only meaningful when `config.engine.default` is `EngineChoice::OpenAi`.
/// Nova ignores `SessionConfig.voice` entirely and hardcodes its own
/// `voiceId` ("tiffany") in `uia-nova/src/protocol.rs`.
///
/// `marin` is `gpt-realtime-2.1`'s own default (confirmed live, see
/// docs/superpowers/findings/2026-08-16-provider-formats.md §3.2) — set
/// explicitly here rather than left unset so a future model-family default
/// change on OpenAI's side can't silently change our voice out from under us.
///
/// Alternatives, all valid for `gpt-realtime-2.1` / `gpt-realtime-2.1-mini`:
/// "alloy", "ash", "ballad", "cedar", "coral", "echo", "sage", "shimmer", "verse".
/// `cedar` is the other newer higher-quality voice alongside `marin`; the rest
/// predate the `gpt-realtime` family. Voice is locked in once the model has
/// emitted its first audio in a session, so this only ever takes effect at
/// connect time.
pub const DEFAULT_OPENAI_VOICE: &str = "marin";

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("configuration: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("engine: {0}")]
    Engine(#[from] uia_core::engine::EngineError),
    #[error("audio device: {0}")]
    Audio(#[from] uia_core::audio::AudioError),
}

/// Build the requested engine. Constructs only — no network call happens until
/// `S2sEngine::connect`, so this is safe to call offline.
///
/// Takes `choice` explicitly rather than reading `config.engine.default`
/// itself (S18): the caller may be overriding the config-file default with a
/// persisted UI selection (`crate::settings::resolve_engine_choice`), and a
/// function that silently re-reads the config default underneath that
/// override would make the override a lie.
pub fn engine_for(
    config: &Config,
    choice: EngineChoice,
    store: &dyn crate::secrets::SecretStore,
) -> Result<Box<dyn S2sEngine>, AppError> {
    Ok(match choice {
        EngineChoice::OpenAi => Box::new(uia_openai::OpenAiEngine::new(
            config.openai.resolve_api_key(store)?,
            config.openai.model.clone(),
        )),
        EngineChoice::Bedrock => {
            // Validated at load time too; repeated here so a Session built
            // from a hand-made Config cannot skip it.
            uia_nova::NovaEngine::validate_region(&config.bedrock.region)?;
            let (access_key_id, secret_access_key) = config.bedrock.resolve_credentials(store)?;
            Box::new(uia_nova::NovaEngine::new(
                config.bedrock.region.clone(),
                access_key_id,
                secret_access_key,
                config.bedrock.model.clone(),
            ))
        }
        EngineChoice::Foundry => {
            let endpoint = config.foundry.resolve_endpoint().ok_or_else(|| {
                crate::config::ConfigError::Invalid(
                    "no Foundry endpoint found (uia.toml [foundry] endpoint, or an \
                     `endpoint:` line in foundry.key_file)"
                        .into(),
                )
            })?;
            Box::new(uia_foundry::FoundryEngine::new(
                endpoint,
                config.foundry.resolve_api_key(store)?,
                config.foundry.deployment.clone(),
            ))
        }
    })
}

/// What `mcp_targets` worked out: the servers to connect, and the ones that
/// were dropped before any connection was attempted, each with the reason.
///
/// `skipped` exists because "it isn't in the list" and "it is broken" are
/// different facts about a server the user explicitly enabled, and the panel
/// has to be able to tell them apart. Before this, a skipped local server was
/// indistinguishable in the UI from a healthy one.
pub struct McpPlan {
    pub targets: Vec<(String, McpServerConfig)>,
    pub skipped: Vec<(String, String)>,
}

/// What one server is contributing to the session that is running now.
///
/// Written from two places, answering two different questions: `build_executor`
/// records whether the server *connected*, once, at startup — and
/// `health_listing_observer` records whether it *contributed tools*, on every
/// `Session::connect`, which is each reconnect, the rotation and every persona
/// switch. The second overwrites the first, deliberately: an hour-old
/// `Connected` must not outlive a listing that got nothing back.
///
/// Still not live in the other direction — nothing here re-reads the registry,
/// so a server added or enabled since startup is simply absent from the map.
/// The Settings panel is responsible for showing an absent server as "pending
/// restart" rather than as healthy — see `SettingsMcp.svelte`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerHealth {
    Connected,
    NoTools { message: String },
    Failed { message: String },
}

/// Shared so the Tauri command layer can read what the session task wrote.
/// `BTreeMap` for a stable order, matching how the registry is serialized.
///
/// Written on every connect, not only at startup, so the mutex is held across
/// a listing that a persona switch is waiting on — kept short for that reason:
/// `record_health` extends the map and returns.
pub type McpHealth = Arc<std::sync::Mutex<std::collections::BTreeMap<String, ServerHealth>>>;

/// A handle the router can report listing outcomes through, writing them into
/// the same map Settings reads.
///
/// This is the whole reason `ListingObserver` is a trait: `uia-mcp` must not
/// learn about `McpHealth`, so the adapter lives on the app's side of the
/// seam and the router only ever sees the trait.
fn health_listing_observer(health: &McpHealth) -> Arc<dyn uia_mcp::ListingObserver> {
    struct Observer(McpHealth);
    impl uia_mcp::ListingObserver for Observer {
        fn record(&self, server: &str, outcome: uia_mcp::ListingOutcome) {
            record_health(
                &self.0,
                [(server.to_string(), health_from_listing(outcome))],
            );
        }
    }
    Arc::new(Observer(Arc::clone(health)))
}

/// Build the router a session runs on, with its listing observer attached.
///
/// One function so the observer cannot be forgotten: a router built without
/// it still works and still routes, and the only symptom is Settings quietly
/// going back to reporting an hour-old connection.
fn route(
    servers: Vec<(String, Arc<dyn ToolExecutor>)>,
    health: &McpHealth,
) -> Arc<dyn ToolExecutor> {
    Arc::new(McpRouter::new(servers).with_listing_observer(health_listing_observer(health)))
}

/// What one listing outcome means for the Status column.
///
/// Every way of contributing nothing collapses into `NoTools`, with the
/// reason kept: to a human reading the panel, "it listed none", "listing
/// failed" and "it never answered" differ only in the sentence beneath the
/// row, and all three mean the same thing — the assistant does not have
/// those tools right now.
fn health_from_listing(outcome: uia_mcp::ListingOutcome) -> ServerHealth {
    match outcome {
        uia_mcp::ListingOutcome::Listed { tools: 0 } => ServerHealth::NoTools {
            message: "it connected, but listed no tools".into(),
        },
        uia_mcp::ListingOutcome::Listed { .. } => ServerHealth::Connected,
        uia_mcp::ListingOutcome::Failed { message } => ServerHealth::NoTools {
            message: format!("it connected, but listing its tools failed: {message}"),
        },
        uia_mcp::ListingOutcome::TimedOut { after } => ServerHealth::NoTools {
            message: format!("it connected, but did not list its tools within {after:?}"),
        },
    }
}

/// The weather bundle's own declared name (`ai-agent-assets/mcp-servers/
/// open-meteo-mcp/manifest.json`'s `"name"` field, copied verbatim into
/// `LocalServerEntry.name` by `install_bundle`) — not a value invented here.
/// Matching on it is how `mcp_targets` knows which local server, if any, is
/// the one that reads `OPEN_METEO_DEFAULT_LOCATION`.
pub const OPEN_METEO_SERVER_NAME: &str = "open-meteo-mcp";

/// Verified against `open-meteo-mcp/src/config.rs::Config::from_vars`. Do not
/// rename this without checking that file — a name mismatch here fails
/// silently: the server just falls back to asking the user.
pub const OPEN_METEO_DEFAULT_LOCATION_ENV: &str = "OPEN_METEO_DEFAULT_LOCATION";

/// What to connect, and what was dropped on the way, in registry order:
/// enabled local servers first, then enabled remote servers.
///
/// Split out from `build_executor` so the mapping is testable without
/// spawning or connecting anything. `skipped` is returned rather than
/// recorded through a shared handle for the same reason: this function stays
/// pure, and the caller decides where the reasons go.
///
/// Every enabled local server is re-validated here, at launch, with the same
/// `validate_bundle` the installer ran — the binary-only rule is a property
/// of what is about to run, not a one-time gate at install.
///
/// A local server whose extracted files are missing or that no longer passes
/// validation is logged and skipped rather than failing the whole list: the
/// registry is a record of what was approved, and the files can go away
/// underneath it. That matches how `build_executor` already treats a
/// server that refuses to connect.
pub fn mcp_targets(
    registry: &McpRegistry,
    local_servers_dir: &std::path::Path,
    // The General tab's configured default place, already the UI's raw
    // value — trimming and blank-means-unset happen here, not upstream, so
    // every caller (including a future one) gets the same rule for free.
    home_location: Option<&str>,
    // Where a bundle's saved `sensitive` settings live. Only read for local
    // servers that declare `user_config`.
    secrets: &dyn crate::secrets::SecretStore,
) -> McpPlan {
    let mut targets = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let home_location = home_location.map(str::trim).filter(|s| !s.is_empty());

    let path_vars = crate::mcp_registry::mcp_path_vars();

    for server in registry.local_servers.iter().filter(|p| p.enabled) {
        let dir = local_servers_dir.join(&server.name);
        let manifest = match std::fs::read_to_string(dir.join("manifest.json")) {
            Ok(raw) => raw,
            Err(e) => {
                let why = format!("its installed files are missing: {e}");
                eprintln!("mcp: local server {:?} skipped, {why}", server.name);
                skipped.push((server.name.clone(), why));
                continue;
            }
        };
        let parsed = match uia_mcp::bundle::parse_manifest(&manifest) {
            Ok(m) => m,
            Err(e) => {
                let why = format!("its manifest.json is unreadable: {e}");
                eprintln!("mcp: local server {:?} skipped, {why}", server.name);
                skipped.push((server.name.clone(), why));
                continue;
            }
        };
        // Deliberately `validate_bundle`, not `resolve_launch`: the platform
        // refusal and the four trust checks run again on EVERY launch, not
        // just at install. The manifest is a file in the config directory,
        // and anything that can write one file there afterwards — another app, a restored or
        // synced config folder, the server's own binary rewriting its own
        // manifest — could otherwise turn an approved local server into
        // `command: "cmd.exe"` with nothing to stop it. "We already
        // validated this at install" is true and beside the point: install
        // validated the bytes that were there then, and this validates the
        // bytes about to be executed now. Do not weaken this to a bare
        // resolve.
        match uia_mcp::bundle::validate_bundle(&parsed, uia_mcp::bundle::current_platform(), &dir) {
            Ok(launch) => {
                // A bundle installed before install restored unix modes sits
                // on disk 0644 and would fail to spawn with EACCES. A chmod
                // failure must not skip the server: the spawn then reports
                // the real error. Never log setting values here.
                if let Err(e) =
                    uia_mcp::bundle::ensure_executable(std::path::Path::new(&launch.command))
                {
                    eprintln!(
                        "mcp: local server {:?}: could not make its command executable: {e}",
                        server.name
                    );
                }
                let values = match crate::mcp_registry::local_config_values(
                    server,
                    &parsed.user_config,
                    secrets,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        let why = format!("its settings could not be read: {e}");
                        eprintln!("mcp: local server {:?} skipped, {why}", server.name);
                        skipped.push((server.name.clone(), why));
                        continue;
                    }
                };
                let launch = match uia_mcp::bundle::apply_user_config(
                    launch,
                    &parsed.user_config,
                    &values,
                    &path_vars,
                ) {
                    Ok(l) => l,
                    Err(e) => {
                        let why = format!("its settings are incomplete: {e}");
                        eprintln!("mcp: local server {:?} skipped, {why}", server.name);
                        skipped.push((server.name.clone(), why));
                        continue;
                    }
                };
                let mut env = launch.env;
                // Only the weather server's own process gets this — an env
                // var is per-process anyway, but the name check is what keeps
                // the intent explicit rather than accidental.
                if server.name == OPEN_METEO_SERVER_NAME
                    && let Some(value) = home_location
                {
                    env.push((
                        OPEN_METEO_DEFAULT_LOCATION_ENV.to_string(),
                        value.to_string(),
                    ));
                }
                targets.push((
                    server.name.clone(),
                    McpServerConfig::Stdio {
                        command: launch.command,
                        args: launch.args,
                        env,
                    },
                ));
            }
            Err(e) => {
                let why = format!("it no longer passes bundle validation: {e}");
                eprintln!("mcp: local server {:?} skipped, {why}", server.name);
                skipped.push((server.name.clone(), why));
            }
        }
    }

    for remote in registry.remote_servers.iter().filter(|r| r.enabled) {
        targets.push((remote.name.clone(), remote.transport()));
    }

    McpPlan { targets, skipped }
}

/// Connect every approved MCP server and fan them out behind one executor.
///
/// A server that refuses to connect is logged and skipped, never fatal: the
/// assistant without one tool is still an assistant.
///
/// OAuth remotes are connected separately from `mcp_targets`'s generic list,
/// via `mcp_registry::connect_oauth_remote`: the plain `McpExecutor::connect`
/// this loop uses for everything else always sends `NoToken` — no bearer
/// token, ever — which is correct for a local server or a static-header remote but
/// silently strips the one thing an OAuth remote needs. Excluded here by
/// name rather than by not calling `mcp_targets` for them at all, so that
/// function's own `enabled` gating stays the single place that decides
/// which servers connect at all.
pub async fn build_executor(
    registry: &McpRegistry,
    local_servers_dir: &std::path::Path,
    health: &McpHealth,
    home_location: Option<&str>,
    secrets: &dyn crate::secrets::SecretStore,
) -> Arc<dyn ToolExecutor> {
    let oauth_remote_names: std::collections::HashSet<&str> = registry
        .remote_servers
        .iter()
        .filter(|r| r.enabled && r.auth == crate::mcp_registry::RemoteAuth::OAuth)
        .map(|r| r.name.as_str())
        .collect();

    let plan = mcp_targets(registry, local_servers_dir, home_location, secrets);

    // Recorded before any connection is attempted: a server dropped by
    // `mcp_targets` never reaches the loop below, so this is the only place
    // its reason can be captured.
    record_health(
        health,
        plan.skipped
            .into_iter()
            .map(|(name, message)| (name, ServerHealth::Failed { message })),
    );

    let mut servers: Vec<(String, Arc<dyn ToolExecutor>)> = Vec::new();
    for (name, transport) in plan.targets {
        if oauth_remote_names.contains(name.as_str()) {
            continue;
        }
        match McpExecutor::connect(&name, transport).await {
            Ok(exec) => {
                record_health(health, [(name.clone(), ServerHealth::Connected)]);
                servers.push((name, Arc::new(exec)));
            }
            Err(e) => {
                // The server's own output was already echoed line by line as it
                // arrived; only Status needs the `; the server printed:` copy.
                let text = e.to_string();
                let on_terminal = text
                    .split_once("; the server printed: ")
                    .map_or(text.as_str(), |(head, _)| head);
                eprintln!("mcp: server {name:?} did not connect, skipping it: {on_terminal}");
                record_health(
                    health,
                    [(
                        name,
                        ServerHealth::Failed {
                            message: e.to_string(),
                        },
                    )],
                );
            }
        }
    }

    // `oauth_remote_names` is already filtered to `enabled` remotes, so this
    // loop inherits the same per-server trust decision `mcp_targets` applies
    // to everything else.
    for remote in registry
        .remote_servers
        .iter()
        .filter(|r| oauth_remote_names.contains(r.name.as_str()))
    {
        match crate::mcp_registry::connect_oauth_remote(remote).await {
            Ok(exec) => {
                record_health(health, [(remote.name.clone(), ServerHealth::Connected)]);
                servers.push((remote.name.clone(), Arc::new(exec)));
            }
            Err(e) => {
                eprintln!(
                    "mcp: server {:?} did not connect, skipping it: {e}",
                    remote.name
                );
                record_health(
                    health,
                    [(
                        remote.name.clone(),
                        ServerHealth::Failed {
                            message: e.to_string(),
                        },
                    )],
                );
            }
        }
    }

    route(servers, health)
}

/// Writes outcomes into the shared map, tolerating a poisoned lock.
///
/// A panic somewhere else must not take the session down with it: health is
/// a display convenience, and losing it is strictly better than refusing to
/// start an assistant that would otherwise work.
fn record_health(health: &McpHealth, entries: impl IntoIterator<Item = (String, ServerHealth)>) {
    let Ok(mut map) = health.lock() else {
        eprintln!("mcp: health map is poisoned, not recording server status");
        return;
    };
    map.extend(entries);
}

/// The `(source, sink)` pair `build_session` wires into `SessionDeps`.
///
/// Named for the same reason `uia-audio`'s `WrappedAudioPair` is: the bare
/// tuple trips clippy's `type_complexity`.
type AudioPair = (Box<dyn AudioSource>, Box<dyn AudioSink>);

/// Whether to open the OS's own echo-cancelling devices (WASAPI's
/// Communications category on Windows, Voice Processing IO on macOS) or plain
/// devices with none.
///
/// Kept outside the platform `cfg`s deliberately: it is the only part of
/// [`os_aec_or_fallback_cpal`]'s decision that doesn't touch real hardware,
/// so it stays testable on every platform, Linux CI included.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn should_use_os_echo_cancellation(config: &Config) -> bool {
    config.audio.aec_enabled
}

/// What the logs call the OS echo-cancellation path on this platform.
#[cfg(windows)]
const OS_AEC_NAME: &str = "Windows communications audio devices";
#[cfg(target_os = "macos")]
const OS_AEC_NAME: &str = "macOS voice-processing audio devices";

/// Open the OS echo-cancelling pair. Either half failing takes both down: a
/// cancelling capture paired with a non-cancelling render is the misaligned
/// case that makes echo worse, not better.
#[cfg(windows)]
fn open_os_aec(
    input: Option<&str>,
    output: Option<&str>,
) -> Result<AudioPair, uia_core::audio::AudioError> {
    use uia_audio::wasapi::{WasapiSink, WasapiSource};
    let source = WasapiSource::input_communications(input)?;
    let sink = WasapiSink::output_communications(output)?;
    Ok((Box::new(source), Box::new(sink)))
}

/// One Voice Processing IO unit serves both directions, so this is one call.
#[cfg(target_os = "macos")]
fn open_os_aec(
    input: Option<&str>,
    output: Option<&str>,
) -> Result<AudioPair, uia_core::audio::AudioError> {
    let (source, sink) = uia_audio::coreaudio::open_voice_processing(input, output)?;
    Ok((Box::new(source), Box::new(sink)))
}

/// Open the OS-native echo-cancelling devices, falling back to plain cpal
/// ones if that fails for any reason — or if `config.audio.aec_enabled` is
/// `false`, in which case plain devices are opened directly and the OS path
/// is never attempted.
///
/// The OS applies its own AEC/AGC/NS beneath the app on this path, so there
/// is nothing for [`wrap_with_aec_if_rates_match`] to do — running AEC3 over
/// already-cancelled audio would filter it twice. The two are mutually
/// exclusive, and `build_session` enforces that by construction.
///
/// The fallback is deliberately to *plain* devices, never to the `aec` path
/// even where it is compiled in: one fallback tier is testable, two is a coin
/// toss about which engine is actually running. `config.audio.*_device`, if
/// set, is honoured on the fallback too — a pinned name is a statement about
/// which physical device to use, not about the echo-cancellation path.
///
/// Gated on the OS alone, deliberately — **not** on `uia-audio`'s
/// `wasapi-aec`/`coreaudio-aec` features, which this crate cannot see.
/// `Cargo.toml`'s per-OS target stanzas activate them unconditionally, which
/// makes "this OS" and "the backend module exists" the same condition.
/// Remove a stanza and this stops compiling — loudly, which is the intent.
#[cfg(any(windows, target_os = "macos"))]
fn os_aec_or_fallback_cpal(config: &Config) -> Result<AudioPair, AppError> {
    let input_device = config.audio.input_device.as_deref();
    let output_device = config.audio.output_device.as_deref();

    if !should_use_os_echo_cancellation(config) {
        eprintln!(
            "uia: OS echo cancellation disabled via config (audio.aec_enabled = false); \
             opening plain audio devices"
        );
        return Ok((
            Box::new(uia_audio::input::CpalSource::input(input_device)?),
            Box::new(uia_audio::output::CpalSink::output(output_device)?),
        ));
    }

    match open_os_aec(input_device, output_device) {
        Ok(pair) => Ok(pair),
        Err(reason) => {
            eprintln!(
                "uia: could not open the {OS_AEC_NAME} ({reason}); falling back to plain \
                 devices WITHOUT echo cancellation — the assistant may hear itself through \
                 speakers"
            );
            Ok((
                Box::new(uia_audio::input::CpalSource::input(input_device)?),
                Box::new(uia_audio::output::CpalSink::output(output_device)?),
            ))
        }
    }
}

/// Wraps `source`/`sink` for echo cancellation, unless their devices' native
/// rates disagree.
///
/// `EchoCancellerConfig` carries one `sample_rate_hz` for both streams: the
/// far-end reference (`sink.write`) and the near-end capture (`source.next_frame`)
/// are aligned frame-for-frame at that single rate (S15's `aec.rs`). If the
/// input and output devices negotiated different native rates, wrapping at
/// either rate would feed the processor a far-end reference at the wrong
/// speed relative to the near-end signal — worse than no cancellation at all,
/// since a misaligned reference can suppress the wrong parts of the real
/// voice. Skipping is silent-safe; mismatched-and-wrapped is not.
#[cfg(all(feature = "aec", not(any(windows, target_os = "macos"))))]
fn wrap_with_aec_if_rates_match(
    source: Box<dyn AudioSource>,
    sink: Box<dyn AudioSink>,
) -> Result<(Box<dyn AudioSource>, Box<dyn AudioSink>), AppError> {
    use uia_audio::aec::{EchoCancellerConfig, wrap_with_echo_cancellation};

    let capture_rate = source.format().sample_rate_hz;
    let playback_rate = sink.format().sample_rate_hz;
    if capture_rate != playback_rate {
        eprintln!(
            "uia: AEC needs matching capture/playback device rates (got {capture_rate} Hz \
             capture vs {playback_rate} Hz playback); skipping echo cancellation"
        );
        return Ok((source, sink));
    }
    let cfg = EchoCancellerConfig {
        enabled: true,
        sample_rate_hz: capture_rate,
    };
    match wrap_with_echo_cancellation(source, sink, cfg) {
        Ok(wrapped) => Ok(wrapped),
        // `wrap_with_echo_cancellation` consumes `source`/`sink` and does not
        // hand them back on error (`EchoCanceller::new` fails before either
        // wrapper type exists), so recovering means reopening fresh devices
        // rather than reusing the originals — they are already gone.
        Err(e) => {
            eprintln!(
                "uia: echo canceller failed to initialise ({e}), reopening \
                 the audio devices without it"
            );
            let source: Box<dyn AudioSource> =
                Box::new(uia_audio::input::CpalSource::default_input()?);
            let sink: Box<dyn AudioSink> = Box::new(uia_audio::output::CpalSink::default_output()?);
            Ok((source, sink))
        }
    }
}

/// The whole thing. Opens the default capture and playback devices, so it needs
/// real audio hardware — which is why it has no offline test and why every
/// piece it assembles is separately testable above.
///
/// `engine_choice` is the caller's resolved choice (config default, or a
/// persisted UI selection overriding it — see `crate::settings`), not
/// `config.engine.default` read again here, for the same reason `engine_for`
/// takes it explicitly. The persona `book` and `global_name` are likewise
/// resolved by the caller (`crate::personas::load_personas` and the General
/// tab's `AgentSettings::name`): which persona the app *starts* as is
/// restart-to-switch, matching FD5. Switching by voice afterwards is a
/// running-session affair and changes nothing on disk.
// `#[allow(clippy::too_many_arguments)]`: this is the composition root, and
// every parameter is a distinct thing the caller resolved at startup. A
// struct wrapping them would only move the same list one line up while
// hiding which of them are restart-to-apply — same reasoning as
// `mcp_registry::remote_entry_from`.
#[allow(clippy::too_many_arguments)]
pub async fn build_session(
    config: &Config,
    engine_choice: EngineChoice,
    activation: Box<dyn Activation>,
    store: &dyn crate::secrets::SecretStore,
    // The whole persona book, not a resolved persona: the session needs the
    // active one for its prompt AND every other one for `switch_persona`.
    book: &crate::personas::PersonaBook,
    // The General tab's identity field. A persona's own `name_override`
    // wins over it; empty means the assistant is called by this name in
    // every persona, which is the ordinary case.
    global_name: Option<&str>,
    // The handle the shell also holds. Taken here rather than set by the
    // caller afterwards because the `switch_persona` decorator needs the
    // very same handle the run loop reads swaps from -- see `persona_tools`.
    control: uia_core::session::SessionControl,
    // Where to keep the conversation. `None` means keep none -- either the
    // General tab's toggle is off, or `[memory] backend` is `none`, or this is
    // a contract/A-B test, which assert on what the engine is sent.
    memory_path: Option<std::path::PathBuf>,
    // What MCP servers this install has approved, and where the installed
    // local server bundles live. Read once at startup by the caller, like the
    // engine choice: enabling a server in Settings applies on next launch.
    registry: &McpRegistry,
    local_servers_dir: &std::path::Path,
    // The General tab's configured default place, passed straight through to
    // `build_executor` — see `mcp_targets` for what happens to it.
    home_location: Option<&str>,
    // Where each server's connect outcome is recorded, for the Settings
    // panel to read back. Write-only from here.
    mcp_health: &McpHealth,
) -> Result<Session, AppError> {
    // The two echo-cancellation paths are mutually exclusive, and the `not(...)`
    // here is what makes that a compile-time guarantee rather than a rule
    // someone has to remember: without it the `aec` call site would still be
    // compiled in on Windows or macOS for anyone who also passed
    // `--features aec`, and the assistant's audio would be filtered twice —
    // once by the OS beneath the app, once by AEC3 on top of it.
    #[cfg(any(windows, target_os = "macos"))]
    let (source, sink) = os_aec_or_fallback_cpal(config)?;

    #[cfg(not(any(windows, target_os = "macos")))]
    let (source, sink): AudioPair = {
        let source: Box<dyn AudioSource> = Box::new(uia_audio::input::CpalSource::input(
            config.audio.input_device.as_deref(),
        )?);
        let sink: Box<dyn AudioSink> = Box::new(uia_audio::output::CpalSink::output(
            config.audio.output_device.as_deref(),
        )?);
        #[cfg(feature = "aec")]
        let (source, sink) = wrap_with_aec_if_rates_match(source, sink)?;
        (source, sink)
    };

    let mut session = Session::new(SessionDeps {
        source,
        sink,
        engine: engine_for(config, engine_choice, store)?,
        executor: persona_tools(
            time_tools(
                build_executor(
                    registry,
                    local_servers_dir,
                    mcp_health,
                    home_location,
                    store,
                )
                .await,
            ),
            book,
            global_name,
            control.clone(),
        ),
        // The resolved setting decides, not the presence of a path: a user who
        // turned memory off must get no history even though the app knows
        // perfectly well where it would have gone.
        memory: match &memory_path {
            Some(path) => Arc::new(crate::memory::FileMemory::load(path.clone()))
                as Arc<dyn uia_core::memory::ConversationMemory>,
            None => Arc::new(NullMemory),
        },
        activation,
        ctx: MemoryContext {
            user_id: "local".into(),
            conversation_id: "sp1".into(),
        },
    });
    // The same handle `persona_tools` was given above, so a swap the
    // decorator requests is one this session takes. Set before `run()`, and
    // before anything subscribes, for the same reason the shell used to set
    // it here: a session running on its own default control is a session
    // whose switches go nowhere.
    session.set_control(control);
    session.set_system_prompt(system_prompt_for_book(engine_choice, book, global_name));
    // A persona switched by voice lives for this process only. It is
    // deliberately never written back to `uia-personas.json`: the sidecar's
    // `active` is the identity the app *launches* as, and only the Personas
    // tab changes that. So there is no persist task here -- the swap
    // surviving in `Session::system_prompt` across rotations and reconnects
    // is the entire persistence story.
    //
    // A swap that fails to land is not lost track of: `apply_pending_prompt_swap`
    // restores the old prompt and reports the failure on the control's
    // prompt-swap outcome watch, which `PersonaExecutor` drains at the top of
    // its next `execute` and uses to drop the switch it recorded
    // optimistically. Reported, never re-queued.
    // Transcription follows memory, for the same reason the memory port above
    // does: it is the thing that consumes transcripts, so paying for them with
    // nothing to store them in would be pure cost. `memory_path` is already
    // the *resolved* setting rather than a mere path -- `None` is how "the
    // user turned memory off" reaches this function.
    //
    // Only the user's half rides on this. Every engine that speaks emits its
    // own output transcript regardless, which is why a store written before
    // this line existed holds assistant replies and not one word the user
    // said. Foundry emits neither half and is unaffected either way.
    session.set_transcription(memory_path.is_some());
    // Hold the connection for as long as the window is up. Zero is the
    // documented opt-out, not an accident (`Session::set_idle_deadline`).
    //
    // The five-minute default existed to beat the provider to an idle close --
    // "being disconnected on our own terms is a state we can show", per its own
    // comment -- but `set_max_session_duration` below already does that job,
    // and does it better: at the provider's real ceiling rather than a guess,
    // and by rotating rather than parking, so the conversation survives it.
    // Two guards against the same failure, the cruder one firing first.
    //
    // It also cost the user the thing they actually notice. The app now starts
    // connected with the mic muted, so the deadline began at launch (`connect`
    // marks activity; a muted frame deliberately does not) and dropped the
    // session five minutes later, having never been spoken to. Leave the app
    // open, come back, find it cold.
    //
    // What this gives up: `mute and walk away` no longer releases the
    // connection. An idle realtime session bills nothing -- only tokens are
    // charged, and a muted mic sends none -- so what is held is a socket, and
    // rotation keeps it healthy.
    session.set_idle_deadline(std::time::Duration::ZERO);
    // Per provider, because the three ceilings differ by almost an order of
    // magnitude -- 8 minutes on Bedrock against 60 on OpenAI -- so one shared
    // number would either rotate OpenAI seven times more than it needs to or
    // let Bedrock die mid-sentence. Load-bearing for the line above: with the
    // idle deadline off, this is the *only* thing keeping a long-lived session
    // ahead of the provider's own close.
    session.set_max_session_duration(match engine_choice {
        EngineChoice::OpenAi => crate::config::resolve_max_session_duration(
            config.openai.max_session_duration_secs,
            crate::config::DEFAULT_OPENAI_MAX_SESSION_SECS,
        ),
        EngineChoice::Bedrock => crate::config::resolve_max_session_duration(
            config.bedrock.max_session_duration_secs,
            crate::config::DEFAULT_BEDROCK_MAX_SESSION_SECS,
        ),
        EngineChoice::Foundry => crate::config::resolve_max_session_duration(
            config.foundry.max_session_duration_secs,
            crate::config::DEFAULT_FOUNDRY_MAX_SESSION_SECS,
        ),
    });
    // Foundry's `gpt-realtime-2.1` deployment shares OpenAI's own GA session
    // schema (`uia_foundry::engine`'s doc comment), including the same
    // `audio.output.voice` field this sets - the live contract test didn't
    // exercise `voice` specifically, but Microsoft's own WebRTC quickstart
    // sample sets one the identical way (`session.audio.output.voice`), so
    // this is a documented field, not a guess.
    if matches!(engine_choice, EngineChoice::OpenAi | EngineChoice::Foundry) {
        session.set_voice(DEFAULT_OPENAI_VOICE);
    }
    if let Some(threshold) = config.audio.barge_in_threshold {
        session.set_barge_in_threshold(threshold);
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::engine::EngineId;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn the_configured_engine_is_the_one_built() {
        // Supplies the key inline rather than through OPENAI_API_KEY: the env
        // var is process-global and the harness runs tests in parallel, so a
        // test that mutates it races every other test that reads it.
        let store = crate::secrets::FakeSecretStore::new();
        let openai = engine_for(
            &config("[openai]\napi_key = \"test-key\"\n"),
            EngineChoice::OpenAi,
            &store,
        )
        .unwrap();
        assert_eq!(openai.id(), EngineId::OpenAi);

        let nova = engine_for(
            &config(
                "[bedrock]\naccess_key_id = \"test-key\"\nsecret_access_key = \"test-secret\"\n",
            ),
            EngineChoice::Bedrock,
            &store,
        )
        .unwrap();
        assert_eq!(nova.id(), EngineId::NovaSonic);

        let foundry = engine_for(
            &config("[foundry]\napi_key = \"test-key\"\nendpoint = \"https://example.services.ai.azure.com\"\n"),
            EngineChoice::Foundry,
            &store,
        )
        .unwrap();
        assert_eq!(foundry.id(), EngineId::Foundry);
    }

    #[test]
    fn foundry_without_an_endpoint_anywhere_is_a_clear_config_error() {
        // Unlike the api key, `endpoint` has no keystore/env-var tier - a
        // missing endpoint must fail with a message naming the two places it
        // could have come from, not a confusing downstream connect() failure.
        let store = crate::secrets::FakeSecretStore::new();
        let Err(err) = engine_for(
            &config("[foundry]\napi_key = \"test-key\"\n"),
            EngineChoice::Foundry,
            &store,
        ) else {
            panic!("expected a Config error for a Foundry section with no endpoint");
        };
        assert!(
            matches!(err, AppError::Config(_)),
            "expected a Config error, got {err:?}"
        );
    }

    #[test]
    fn the_explicit_choice_wins_over_the_configs_own_default() {
        // engine_for must obey its `choice` argument, not silently re-read
        // `config.engine.default` underneath it — that is the whole point of
        // taking it explicitly (S18: a persisted UI choice overrides the
        // config-file default).
        let cfg = config(
            "[engine]\ndefault = \"openai\"\n[bedrock]\naccess_key_id = \"test-key\"\nsecret_access_key = \"test-secret\"\n",
        );
        let store = crate::secrets::FakeSecretStore::new();
        let nova = engine_for(&cfg, EngineChoice::Bedrock, &store).unwrap();
        assert_eq!(nova.id(), EngineId::NovaSonic);
    }

    #[test]
    fn a_nova_region_without_nova_sonic_is_refused_before_any_connect() {
        // Belt and braces with `Config::validate`: a Session composed from a
        // hand-built Config must not reach Bedrock and get a confusing
        // ValidationException instead.
        let store = crate::secrets::FakeSecretStore::new();
        let Err(err) = engine_for(
            &config("[bedrock]\nregion = \"ap-southeast-2\"\n"),
            EngineChoice::Bedrock,
            &store,
        ) else {
            panic!("Sydney must not produce a Nova engine");
        };
        assert!(matches!(err, AppError::Engine(_)), "got {err:?}");
    }

    use crate::mcp_registry::{LocalServerEntry, McpRegistry, RemoteEntry};
    use crate::secrets::SecretStore;

    /// A bundle on disk whose manifest uses `user_config`, plus a registry
    /// that has it enabled.
    fn user_config_fixture(label: &str) -> (McpRegistry, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("uia-sess-cfg-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let bundle = dir.join("mymy");
        std::fs::create_dir_all(bundle.join("server")).unwrap();
        std::fs::write(bundle.join("server/mymy"), b"\x7fELF-bytes").unwrap();
        std::fs::write(
            bundle.join("manifest.json"),
            r#"{ "manifest_version":"0.3","name":"mymy","version":"1",
                 "server":{"type":"binary","entry_point":"server/mymy",
                   "mcp_config":{"command":"${__dirname}/server/mymy",
                     "env":{"TOASTS":"${user_config.toasts}","TOKEN":"${user_config.token}"}}},
                 "user_config":{
                   "toasts":{"type":"string","default":"true"},
                   "token":{"type":"string","sensitive":true,"required":true}}}"#,
        )
        .unwrap();
        let mut reg = McpRegistry::default();
        reg.add_local_server("mymy".into(), Some("1".into()))
            .unwrap();
        reg.set_local_server_enabled("mymy", true).unwrap();
        (reg, dir)
    }

    #[test]
    fn saved_settings_reach_the_launched_servers_environment() {
        let (mut reg, dir) = user_config_fixture("ok");
        reg.local_servers[0]
            .user_config
            .insert("toasts".into(), "false".into());
        let secrets = crate::secrets::FakeSecretStore::new();
        secrets.set("mcp-config.mymy.token", "tok").unwrap();

        let plan = mcp_targets(&reg, &dir, None, &secrets);
        std::fs::remove_dir_all(&dir).ok();

        let McpServerConfig::Stdio { env, .. } = &plan.targets[0].1 else {
            panic!("not stdio")
        };
        assert!(env.contains(&("TOASTS".into(), "false".into())), "{env:?}");
        assert!(env.contains(&("TOKEN".into(), "tok".into())), "{env:?}");
    }

    fn path_var_fixture(label: &str) -> (McpRegistry, std::path::PathBuf) {
        let (reg, dir) = user_config_fixture(label);
        std::fs::write(
            dir.join("mymy/manifest.json"),
            r#"{ "manifest_version":"0.3","name":"mymy","version":"1",
                 "server":{"type":"binary","entry_point":"server/mymy",
                   "mcp_config":{"command":"${__dirname}/server/mymy",
                     "env":{"ROOT":"${user_config.root}"}}},
                 "user_config":{"root":{"type":"directory","default":"${HOME}/data"}}}"#,
        )
        .unwrap();
        (reg, dir)
    }

    #[test]
    fn a_path_variable_in_a_manifest_default_is_expanded_at_launch() {
        let Some(home) = crate::mcp_registry::mcp_path_vars().get("HOME").cloned() else {
            eprintln!("skipped: no home directory in this environment");
            return;
        };
        let (reg, dir) = path_var_fixture("pathvar-default");
        let plan = mcp_targets(&reg, &dir, None, &crate::secrets::FakeSecretStore::new());
        std::fs::remove_dir_all(&dir).ok();
        let McpServerConfig::Stdio { env, .. } = &plan.targets[0].1 else {
            panic!("not stdio")
        };
        assert!(
            env.contains(&("ROOT".into(), format!("{home}/data"))),
            "{env:?}"
        );
    }

    /// A user-typed value is data: typing `${HOME}` must not read the home dir.
    #[test]
    fn a_typed_path_variable_reaches_the_server_literally() {
        let (mut reg, dir) = path_var_fixture("pathvar-typed");
        reg.local_servers[0]
            .user_config
            .insert("root".into(), "${HOME}".into());
        let plan = mcp_targets(&reg, &dir, None, &crate::secrets::FakeSecretStore::new());
        std::fs::remove_dir_all(&dir).ok();
        let McpServerConfig::Stdio { env, .. } = &plan.targets[0].1 else {
            panic!("not stdio")
        };
        assert!(env.contains(&("ROOT".into(), "${HOME}".into())), "{env:?}");
    }

    /// A bundle installed before install restored unix modes is 0644 on disk
    /// and would fail to spawn with EACCES.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_bundle_binary_is_made_executable_at_launch() {
        use std::os::unix::fs::PermissionsExt;
        let (reg, dir) = user_config_fixture("exec");
        let bin = dir.join("mymy/server/mymy");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        let secrets = crate::secrets::FakeSecretStore::new();
        secrets.set("mcp-config.mymy.token", "tok").unwrap();

        let plan = mcp_targets(&reg, &dir, None, &secrets);
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(plan.targets.len(), 1, "{:?}", plan.skipped);
        assert_ne!(mode & 0o111, 0, "mode was {mode:o}");
    }

    #[test]
    fn a_server_built_for_another_platform_is_skipped_with_the_platform_named() {
        let other = ["win32", "darwin", "linux"]
            .into_iter()
            .find(|p| *p != uia_mcp::bundle::current_platform())
            .unwrap();
        let (reg, dir) = user_config_fixture("otheros");
        std::fs::write(
            dir.join("mymy/manifest.json"),
            format!(
                r#"{{ "manifest_version":"0.3","name":"mymy","version":"1",
                     "compatibility":{{"platforms":["{other}"]}},
                     "server":{{"type":"binary","entry_point":"server/mymy",
                       "mcp_config":{{"command":"${{__dirname}}/server/mymy"}}}}}}"#
            ),
        )
        .unwrap();

        let plan = mcp_targets(&reg, &dir, None, &crate::secrets::FakeSecretStore::new());
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            plan.targets.is_empty(),
            "launched a foreign-platform bundle"
        );
        let [(name, why)] = plan.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped server, got {:?}",
                plan.skipped
            );
        };
        assert_eq!(name, "mymy");
        assert!(why.contains(other), "{why}");
    }

    #[test]
    fn an_unreadable_keyring_skips_the_server_naming_the_keyring_not_the_setting() {
        let (reg, dir) = user_config_fixture("unreadable");
        let plan = mcp_targets(&reg, &dir, None, &crate::secrets::UnreadableSecretStore);
        std::fs::remove_dir_all(&dir).ok();
        assert!(plan.targets.is_empty());
        let [(name, why)] = plan.skipped.as_slice() else {
            panic!("expected one skipped server, got {:?}", plan.skipped);
        };
        assert_eq!(name, "mymy");
        assert!(why.contains("keyring") && why.contains("locked"), "{why}");
    }

    #[test]
    fn a_required_secret_that_is_not_in_the_keyring_fails_the_server_with_a_reason() {
        let (reg, dir) = user_config_fixture("missing");
        let plan = mcp_targets(&reg, &dir, None, &crate::secrets::FakeSecretStore::new());
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            plan.targets.is_empty(),
            "launched with a missing required setting"
        );
        let [(name, why)] = plan.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped server, got {:?}",
                plan.skipped
            );
        };
        assert_eq!(name, "mymy");
        assert!(why.contains("token") && why.contains("Settings"), "{why}");
    }

    /// Lays down a local server directory the way `install_bundle` would have, so
    /// `mcp_targets` has a real manifest to resolve a launch from.
    fn installed_local_server(local_servers_dir: &std::path::Path, name: &str) {
        let dir = local_servers_dir.join(name);
        std::fs::create_dir_all(dir.join("server")).unwrap();
        std::fs::write(dir.join("server").join(name), b"\x7fELF\x02binary").unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{
                    "manifest_version": "0.2",
                    "name": "{name}",
                    "version": "1.0.0",
                    "server": {{
                        "type": "binary",
                        "entry_point": "server/{name}",
                        "mcp_config": {{
                            "command": "${{__dirname}}/server/{name}",
                            "args": ["--stdio"]
                        }}
                    }}
                }}"#
            ),
        )
        .unwrap();
    }

    fn local_servers_root(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("uia-sess-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn remote_entry(name: &str, enabled: bool) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            url: format!("https://{name}.test/mcp"),
            enabled,
            ..Default::default()
        }
    }

    /// An enabled local server is a target; a disabled one is not. The
    /// enable flag is the whole approval mechanism, so it has to bite here.
    #[test]
    fn only_enabled_local_servers_become_targets_and_they_launch_from_their_own_directory() {
        let dir = local_servers_root("local");
        installed_local_server(&dir, "clock");
        installed_local_server(&dir, "files");
        let registry = McpRegistry {
            local_servers: vec![
                LocalServerEntry {
                    name: "clock".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
                LocalServerEntry {
                    name: "files".into(),
                    version: None,
                    enabled: false,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
            ],
            remote_servers: vec![],
        };

        let targets = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        )
        .targets;

        let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
        let transport = targets.first().map(|(_, t)| t.clone());
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(names, vec!["clock".to_string()]);
        assert!(
            matches!(
                &transport,
                Some(McpServerConfig::Stdio { command, args, .. })
                    if command.contains("clock") && args == &["--stdio".to_string()]
            ),
            "got {transport:?}"
        );
    }

    /// The one behavior this whole seam exists for: an enabled local server
    /// named exactly like the weather bundle gets the env var the server
    /// reads at startup. A different server, even one enabled alongside it,
    /// must not receive it — an env var is per-process, but this asserts the
    /// intent explicitly rather than trusting that alone.
    #[test]
    fn home_location_is_injected_only_into_the_open_meteo_servers_env() {
        let dir = local_servers_root("home-loc");
        installed_local_server(&dir, OPEN_METEO_SERVER_NAME);
        installed_local_server(&dir, "clock");
        let registry = McpRegistry {
            local_servers: vec![
                LocalServerEntry {
                    name: OPEN_METEO_SERVER_NAME.into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
                LocalServerEntry {
                    name: "clock".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
            ],
            remote_servers: vec![],
        };

        let targets = mcp_targets(
            &registry,
            &dir,
            Some("Hobart"),
            &crate::secrets::FakeSecretStore::new(),
        )
        .targets;
        std::fs::remove_dir_all(&dir).ok();

        let env_of = |name: &str| {
            targets
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, cfg)| match cfg {
                    McpServerConfig::Stdio { env, .. } => Some(env.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(
            env_of(OPEN_METEO_SERVER_NAME).contains(&(
                "OPEN_METEO_DEFAULT_LOCATION".to_string(),
                "Hobart".to_string()
            )),
            "got {:?}",
            env_of(OPEN_METEO_SERVER_NAME)
        );
        assert!(
            !env_of("clock")
                .iter()
                .any(|(k, _)| k == "OPEN_METEO_DEFAULT_LOCATION"),
            "the env var must not leak into an unrelated server"
        );
    }

    /// No configured value means no env var at all — not an empty one. An
    /// empty `OPEN_METEO_DEFAULT_LOCATION=` would still be `Some("")` on the
    /// server's side of `std::env::var`, so omitting the key entirely is
    /// what actually reproduces "unconfigured" for it.
    #[test]
    fn no_home_location_means_the_env_var_is_never_added() {
        let dir = local_servers_root("home-loc-none");
        installed_local_server(&dir, OPEN_METEO_SERVER_NAME);
        let registry = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: OPEN_METEO_SERVER_NAME.into(),
                version: None,
                enabled: true,
                user_config: std::collections::BTreeMap::new(),
                ..Default::default()
            }],
            remote_servers: vec![],
        };

        let targets = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        )
        .targets;
        std::fs::remove_dir_all(&dir).ok();

        let McpServerConfig::Stdio { env, .. } = &targets[0].1 else {
            panic!("expected a Stdio config");
        };
        assert!(env.is_empty(), "got {env:?}");
    }

    /// A blank or all-whitespace value is treated the same as unset — mirrors
    /// the weather server's own rule for a blank `OPEN_METEO_DEFAULT_LOCATION`
    /// (`open-meteo-mcp/src/config.rs`), applied on this side of the seam too
    /// so a UI field left as spaces doesn't turn into a confusing empty-string
    /// default on the other side of the process boundary.
    #[test]
    fn a_blank_home_location_is_treated_as_unset() {
        let dir = local_servers_root("home-loc-blank");
        installed_local_server(&dir, OPEN_METEO_SERVER_NAME);
        let registry = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: OPEN_METEO_SERVER_NAME.into(),
                version: None,
                enabled: true,
                user_config: std::collections::BTreeMap::new(),
                ..Default::default()
            }],
            remote_servers: vec![],
        };

        let targets = mcp_targets(
            &registry,
            &dir,
            Some("   "),
            &crate::secrets::FakeSecretStore::new(),
        )
        .targets;
        std::fs::remove_dir_all(&dir).ok();

        let McpServerConfig::Stdio { env, .. } = &targets[0].1 else {
            panic!("expected a Stdio config");
        };
        assert!(env.is_empty(), "got {env:?}");
    }

    #[test]
    fn only_remotes_that_are_themselves_enabled_become_targets() {
        let dir = local_servers_root("remote-on");
        let registry = McpRegistry {
            local_servers: vec![],
            remote_servers: vec![remote_entry("docs", true), remote_entry("wiki", false)],
        };

        let targets = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        )
        .targets;

        let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
        let transport = targets.first().map(|(_, t)| t.clone());
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(names, vec!["docs".to_string()]);
        assert!(
            matches!(&transport, Some(McpServerConfig::Http { url, .. }) if url.contains("docs")),
            "got {transport:?}"
        );
    }

    /// A local server recorded in the registry whose files are gone must not take
    /// the session down with it — the same "one bad server is not fatal"
    /// rule `build_executor` already applies to one that fails to connect.
    ///
    /// The missing one is listed FIRST, and a healthy one follows it, ON
    /// PURPOSE — do not reorder these. "Skipped" and "aborts the rest of the
    /// list" are different bugs, and with the missing entry last they produce
    /// an identical result: an implementation that `return`ed or `break`d
    /// instead of `continue`ing would still pass. Putting a healthy one on
    /// the far side of the bad one is what makes an early exit visible, as a
    /// truncated list rather than a panic.
    #[test]
    fn a_local_server_whose_files_are_missing_is_skipped_without_dropping_the_ones_after_it() {
        let dir = local_servers_root("missing");
        installed_local_server(&dir, "clock");
        installed_local_server(&dir, "files");
        let registry = McpRegistry {
            local_servers: vec![
                LocalServerEntry {
                    name: "clock".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
                LocalServerEntry {
                    name: "vanished".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
                LocalServerEntry {
                    name: "files".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
            ],
            remote_servers: vec![],
        };

        let targets = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        )
        .targets;

        let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            names,
            vec!["clock".to_string(), "files".to_string()],
            "the local server after the missing one must survive: skipping is not aborting"
        );
    }

    /// The binary-only rule has to hold at LAUNCH, not just at install.
    /// Here the manifest is rewritten after installation to declare a
    /// `node` server — exactly what a tampered, restored, or self-rewriting
    /// local server directory looks like — and it must not become a
    /// target. `resolve_launch` alone would happily hand back a command
    /// here; only `validate_bundle` refuses.
    ///
    /// As in the missing-files test, the tampered one is listed FIRST
    /// with a healthy one after it, on purpose: that ordering is what
    /// distinguishes "skipped" from "aborted the whole list".
    #[test]
    fn a_local_server_whose_manifest_was_rewritten_after_install_fails_validation_at_launch() {
        let dir = local_servers_root("tampered");
        installed_local_server(&dir, "clock");
        installed_local_server(&dir, "files");
        // Same bundle, same shipped binary — only the declared type changes,
        // which is all it takes to stop being a reviewable binary.
        std::fs::write(
            dir.join("clock").join("manifest.json"),
            r#"{
                "manifest_version": "0.2",
                "name": "clock",
                "version": "1.0.0",
                "server": {
                    "type": "node",
                    "entry_point": "server/clock",
                    "mcp_config": {
                        "command": "${__dirname}/server/clock",
                        "args": ["--stdio"]
                    }
                }
            }"#,
        )
        .unwrap();
        let registry = McpRegistry {
            local_servers: vec![
                LocalServerEntry {
                    name: "clock".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
                LocalServerEntry {
                    name: "files".into(),
                    version: None,
                    enabled: true,
                    user_config: std::collections::BTreeMap::new(),
                    ..Default::default()
                },
            ],
            remote_servers: vec![],
        };

        let plan = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        );

        let names: Vec<String> = plan.targets.iter().map(|(n, _)| n.clone()).collect();
        let skipped = plan.skipped.clone();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            names,
            vec!["files".to_string()],
            "a local server that no longer validates must be skipped at launch, \
             and the healthy one after it must survive"
        );
        // Skipping quietly is exactly what the panel could not tell apart
        // from working, so the reason travelling out with the plan is the
        // half that matters to the user.
        assert_eq!(skipped.len(), 1, "got {skipped:?}");
        assert_eq!(skipped[0].0, "clock");
        assert!(
            skipped[0].1.contains("no longer passes bundle validation"),
            "the reason must say what was wrong, got {:?}",
            skipped[0].1
        );
    }

    /// The other half of the same guarantee: a server whose directory was
    /// deleted must report WHY it is absent, not merely be absent. Separate
    /// from the tampered case because the two take different branches -- one
    /// fails reading the file, the other fails validating it.
    #[test]
    fn a_local_server_whose_files_are_missing_reports_why_it_was_skipped() {
        let dir = local_servers_root("missing-why");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: "ghost".into(),
                version: None,
                enabled: true,
                user_config: std::collections::BTreeMap::new(),
                ..Default::default()
            }],
            remote_servers: vec![],
        };

        let plan = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        );
        std::fs::remove_dir_all(&dir).ok();

        assert!(plan.targets.is_empty());
        assert_eq!(plan.skipped.len(), 1, "got {:?}", plan.skipped);
        assert_eq!(plan.skipped[0].0, "ghost");
        assert!(
            plan.skipped[0].1.contains("installed files are missing"),
            "got {:?}",
            plan.skipped[0].1
        );
    }

    /// A server that is merely disabled was never asked for, so it is not
    /// "skipped". Conflating the two would put a red failure line under every
    /// server the user deliberately turned off.
    #[test]
    fn a_disabled_local_server_is_absent_without_being_reported_as_skipped() {
        let dir = local_servers_root("disabled-not-skipped");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = McpRegistry {
            local_servers: vec![LocalServerEntry {
                name: "ghost".into(),
                version: None,
                enabled: false,
                user_config: std::collections::BTreeMap::new(),
                ..Default::default()
            }],
            remote_servers: vec![],
        };

        let plan = mcp_targets(
            &registry,
            &dir,
            None,
            &crate::secrets::FakeSecretStore::new(),
        );
        std::fs::remove_dir_all(&dir).ok();

        assert!(plan.targets.is_empty());
        assert!(
            plan.skipped.is_empty(),
            "a disabled server must not be reported as broken, got {:?}",
            plan.skipped
        );
    }

    /// The listing outcomes a running session reports, and what the Status
    /// column is supposed to make of each one.
    ///
    /// These are the mapping, not the plumbing: `build_executor` hands the
    /// router the observer these exercise, and the router is already tested
    /// for producing the outcomes.
    mod listing_outcomes {
        use super::*;
        use uia_mcp::ListingOutcome;

        fn health_with(entries: [(&str, ServerHealth); 1]) -> McpHealth {
            let health = McpHealth::default();
            record_health(
                &health,
                entries.map(|(name, state)| (name.to_string(), state)),
            );
            health
        }

        fn state_of(health: &McpHealth, name: &str) -> ServerHealth {
            health.lock().unwrap().get(name).cloned().unwrap()
        }

        /// A server that connects and then contributes nothing — the case
        /// this whole path exists for.
        struct SilentExecutor;

        #[async_trait::async_trait]
        impl ToolExecutor for SilentExecutor {
            async fn list_tools(
                &self,
            ) -> Result<Vec<uia_core::tools::ToolDescriptor>, uia_core::tools::ToolError>
            {
                Ok(Vec::new())
            }
            async fn execute(
                &self,
                _name: &str,
                _args: serde_json::Value,
                _deadline: std::time::Duration,
            ) -> Result<uia_core::tools::ToolResult, uia_core::tools::ToolError> {
                Err(uia_core::tools::ToolError::Transport("no tools".into()))
            }
        }

        #[tokio::test]
        async fn the_executor_a_session_gets_reports_its_listing_into_the_health_map() {
            // The wiring, not the mapping: proves `build_executor`'s router is
            // the one carrying the observer, so a listing that happens on a
            // reconnect actually reaches Settings.
            let health = McpHealth::default();
            let exec = route(
                vec![(
                    "clock".into(),
                    Arc::new(SilentExecutor) as Arc<dyn ToolExecutor>,
                )],
                &health,
            );

            exec.list_tools().await.unwrap();

            assert!(
                matches!(
                    health.lock().unwrap().get("clock"),
                    Some(ServerHealth::NoTools { .. })
                ),
                "a connected server that listed nothing must say so"
            );
        }

        #[test]
        fn a_server_that_listed_tools_is_recorded_as_connected() {
            let health = McpHealth::default();
            health_listing_observer(&health).record("clock", ListingOutcome::Listed { tools: 3 });

            assert_eq!(state_of(&health, "clock"), ServerHealth::Connected);
        }

        #[test]
        fn a_server_that_listed_nothing_is_recorded_as_contributing_no_tools() {
            // Connecting is not the same as contributing. A server that
            // answers with an empty list leaves the session exactly as
            // toolless as one that never answered.
            let health = McpHealth::default();
            health_listing_observer(&health).record("clock", ListingOutcome::Listed { tools: 0 });

            match state_of(&health, "clock") {
                ServerHealth::NoTools { message } => {
                    assert!(message.contains("no tools"), "got {message:?}")
                }
                other => panic!("expected no-tools, got {other:?}"),
            }
        }

        #[test]
        fn a_listing_failure_is_recorded_as_no_tools_carrying_its_reason() {
            let health = McpHealth::default();
            health_listing_observer(&health).record(
                "weather",
                ListingOutcome::Failed {
                    message: "server is down".into(),
                },
            );

            match state_of(&health, "weather") {
                ServerHealth::NoTools { message } => {
                    assert!(message.contains("server is down"), "got {message:?}")
                }
                other => panic!("expected no-tools, got {other:?}"),
            }
        }

        #[test]
        fn a_listing_timeout_is_recorded_as_no_tools_naming_the_deadline() {
            let health = McpHealth::default();
            health_listing_observer(&health).record(
                "wedged",
                ListingOutcome::TimedOut {
                    after: std::time::Duration::from_secs(2),
                },
            );

            match state_of(&health, "wedged") {
                // A human reading "did not answer" needs to know how long it
                // was given before deciding the server is at fault.
                ServerHealth::NoTools { message } => {
                    assert!(message.contains("2s"), "got {message:?}")
                }
                other => panic!("expected no-tools, got {other:?}"),
            }
        }

        #[test]
        fn a_later_listing_replaces_the_startup_connection_outcome() {
            // The contract change: listing re-runs on every `Session::connect`
            // — each reconnect, each rotation, each persona switch — so a
            // server that connected at launch and went unresponsive an hour
            // later must stop reading as `Connected`.
            let health = health_with([("clock", ServerHealth::Connected)]);
            health_listing_observer(&health).record(
                "clock",
                ListingOutcome::TimedOut {
                    after: std::time::Duration::from_secs(2),
                },
            );

            assert!(
                matches!(state_of(&health, "clock"), ServerHealth::NoTools { .. }),
                "an hour-old Connected must not survive a listing that got nothing"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_registry_yields_an_executor_that_declares_no_tools() {
        let dir = local_servers_root("empty");
        let exec = build_executor(
            &McpRegistry::default(),
            &dir,
            &McpHealth::default(),
            None,
            &crate::secrets::FakeSecretStore::new(),
        )
        .await;
        let empty = exec.list_tools().await.unwrap().is_empty();
        std::fs::remove_dir_all(&dir).ok();
        assert!(empty);
    }

    fn plain_persona(archetype: &str) -> crate::personas::Persona {
        crate::personas::Persona {
            id: "x".into(),
            label: "X".into(),
            archetype: archetype.into(),
            manner: "Plain and direct.".into(),
            use_when: "Always.".into(),
            ..crate::personas::Persona::default()
        }
    }

    #[test]
    fn the_system_prompt_names_the_engine_it_is_running_on() {
        let persona = plain_persona("a voice assistant");
        let openai = system_prompt_for(EngineChoice::OpenAi, &persona, None, false);
        assert!(openai.contains("OpenAI Realtime"), "got: {openai}");
        assert!(!openai.contains("Nova"), "got: {openai}");

        let nova = system_prompt_for(EngineChoice::Bedrock, &persona, None, false);
        assert!(nova.contains("AWS Nova Sonic"), "got: {nova}");
        assert!(!nova.contains("OpenAI"), "got: {nova}");

        let foundry = system_prompt_for(EngineChoice::Foundry, &persona, None, false);
        assert!(foundry.contains("Microsoft AI Foundry"), "got: {foundry}");
        assert!(!foundry.contains("Nova"), "got: {foundry}");
    }

    #[test]
    fn the_persona_shapes_the_body_of_the_prompt() {
        let prompt = system_prompt_for(
            EngineChoice::OpenAi,
            &plain_persona("a pirate"),
            None,
            false,
        );
        assert!(
            prompt.starts_with(&format!(
                "Your name is {DEFAULT_ASSISTANT_NAME}. You are a pirate."
            )),
            "got: {prompt}"
        );
        assert!(prompt.contains("OpenAI Realtime"), "got: {prompt}");
    }

    #[test]
    fn a_configured_name_replaces_the_default_name() {
        let prompt = system_prompt_for(
            EngineChoice::OpenAi,
            &plain_persona("a voice assistant"),
            Some("Jarvis"),
            false,
        );
        assert!(prompt.starts_with("Your name is Jarvis."), "got: {prompt}");
        assert!(!prompt.contains(DEFAULT_ASSISTANT_NAME), "got: {prompt}");
    }

    #[test]
    fn a_missing_or_whitespace_only_name_falls_back_to_the_default_name() {
        let persona = plain_persona("a voice assistant");
        let expected = format!("Your name is {DEFAULT_ASSISTANT_NAME}.");
        assert!(
            system_prompt_for(EngineChoice::OpenAi, &persona, None, false).starts_with(&expected)
        );
        assert!(
            system_prompt_for(EngineChoice::OpenAi, &persona, Some("   "), false)
                .starts_with(&expected)
        );
    }

    #[test]
    fn the_brevity_instruction_reaches_every_engine_prompt() {
        for choice in [
            EngineChoice::OpenAi,
            EngineChoice::Bedrock,
            EngineChoice::Foundry,
        ] {
            let prompt =
                system_prompt_for(choice, &plain_persona("a voice assistant"), None, false);
            assert!(
                prompt.contains(BREVITY_INSTRUCTION),
                "{choice:?} prompt lost the brevity instruction: {prompt}"
            );
        }
    }

    #[test]
    fn no_prompt_invites_the_assistant_to_introduce_itself() {
        // It presented itself on connect: a spoken self-introduction nobody
        // asked for, which also landed in the conversation store as an
        // exchange with an assistant half and no user half.
        for choice in [
            EngineChoice::OpenAi,
            EngineChoice::Bedrock,
            EngineChoice::Foundry,
        ] {
            let prompt =
                system_prompt_for(choice, &plain_persona("a voice assistant"), None, false);
            assert!(
                prompt.contains(NO_SELF_INTRODUCTION),
                "{choice:?} prompt lets the assistant open unprompted"
            );
        }
    }

    #[test]
    fn the_prompt_has_no_stray_whitespace() {
        // Every prompt rides on every connect, so runs of padding are paid
        // for on each one. A line continuation that loses its backslash is
        // the easy way to introduce them and is invisible in review.
        let prompt = system_prompt_for(
            EngineChoice::OpenAi,
            &plain_persona("a voice assistant"),
            Some("Jarvis"),
            false,
        );
        assert!(
            !prompt.contains("  "),
            "doubled whitespace in the prompt: {prompt:?}"
        );
        assert_eq!(prompt.trim(), prompt, "prompt is padded at an end");
    }

    #[test]
    fn the_brevity_instruction_is_one_sentence_about_length() {
        // It is paid for on every connect and S6 measures against this exact
        // string, so a future edit that turns it into a paragraph should fail
        // here rather than quietly cost more than it saves.
        assert!(BREVITY_INSTRUCTION.ends_with('.'), "must be one sentence");
        assert_eq!(
            BREVITY_INSTRUCTION.matches('.').count(),
            1,
            "one sentence, not a paragraph: {BREVITY_INSTRUCTION}"
        );
        assert!(
            BREVITY_INSTRUCTION.split_whitespace().count() <= 20,
            "too long to ride on every setup prompt: {BREVITY_INSTRUCTION}"
        );
    }

    #[test]
    fn aec_enabled_defaults_to_using_os_echo_cancellation() {
        assert!(should_use_os_echo_cancellation(&config("")));
    }

    #[test]
    fn aec_disabled_in_config_reaches_the_os_composition_decision() {
        assert!(!should_use_os_echo_cancellation(&config(
            "[audio]\naec_enabled = false\n"
        )));
    }

    fn persona(id: &str, archetype: &str) -> crate::personas::Persona {
        crate::personas::Persona {
            id: id.into(),
            label: id.into(),
            archetype: archetype.into(),
            manner: format!("Speaks like {id}."),
            use_when: format!("When {id} fits."),
            enabled: true,
            ..Default::default()
        }
    }

    fn a_book_whose_active_is_not_the_first() -> crate::personas::PersonaBook {
        crate::personas::PersonaBook {
            active: "focus".into(),
            allow_agent_switch: false,
            personas: vec![
                persona("default", "a general-purpose voice assistant"),
                persona("focus", "a focus coach"),
                persona("secretary", "a secretary"),
            ],
        }
    }

    /// The whole point of the book: the prompt comes from `active`, which is
    /// neither the first entry nor `default`. Getting this wrong is silent —
    /// the app starts, talks, and is simply the wrong assistant.
    #[test]
    fn the_prompt_is_composed_from_the_books_active_persona() {
        let prompt = system_prompt_for_book(
            EngineChoice::OpenAi,
            &a_book_whose_active_is_not_the_first(),
            None,
        );

        assert!(
            prompt.contains("You are a focus coach."),
            "the active persona's archetype must be the one composed: {prompt}"
        );
        assert!(
            !prompt.contains("general-purpose") && !prompt.contains("secretary"),
            "no other persona may leak into the prompt: {prompt}"
        );
        // The invariants composition exists to guarantee, asserted here too
        // because this is the path a real session actually takes.
        assert!(prompt.contains(BREVITY_INSTRUCTION));
        assert!(prompt.contains(NO_SELF_INTRODUCTION));
    }

    /// `allow_agent_switch` has to reach `compose_prompt` from the book, not
    /// from a hardcoded `false` (which is exactly what the provisional Task 5
    /// wiring passed): without the clause the model is never told it may
    /// switch, so the tool it was offered goes unused.
    #[test]
    fn the_switch_clause_follows_the_books_toggle() {
        let mut book = a_book_whose_active_is_not_the_first();
        assert!(
            !system_prompt_for_book(EngineChoice::OpenAi, &book, None)
                .contains(crate::personas::SWITCH_CLAUSE),
            "switching off means no clause"
        );

        book.allow_agent_switch = true;
        assert!(
            system_prompt_for_book(EngineChoice::OpenAi, &book, None)
                .contains(crate::personas::SWITCH_CLAUSE),
            "switching on must reach the composed prompt"
        );
    }

    /// A book whose `active` names nothing still has to yield an identity:
    /// `load_personas` guarantees `active` resolves, but a book built in a
    /// test or by a future Tauri command has no such guarantee, and a
    /// session with no prompt at all is worse than a default one.
    #[test]
    fn an_unresolvable_active_falls_back_rather_than_panicking() {
        let mut book = a_book_whose_active_is_not_the_first();
        book.active = "no-such-persona".into();
        let prompt = system_prompt_for_book(EngineChoice::OpenAi, &book, None);
        assert!(
            prompt.contains(BREVITY_INSTRUCTION),
            "still a real prompt: {prompt}"
        );
    }

    /// The hazard this task was warned about: `PersonaExecutor` requests the
    /// swap on the handle it was given, and the run loop consumes it from the
    /// handle the session holds. If `build_session` hands the executor a
    /// different `SessionControl` from the one it sets on the session, every
    /// switch is answered and then dropped on the floor.
    ///
    /// Exercised through `persona_tools`, which is the exact call
    /// `build_session` makes, so this covers the wiring rather than merely
    /// re-testing `SessionControl::clone`.
    #[tokio::test]
    async fn the_switch_tool_requests_the_swap_on_the_sessions_own_control() {
        let mut book = a_book_whose_active_is_not_the_first();
        book.allow_agent_switch = true;
        let control = uia_core::session::SessionControl::new();

        let executor = persona_tools(
            Arc::new(uia_core::tools::FakeExecutor::new()) as Arc<dyn ToolExecutor>,
            &book,
            None,
            // What `build_session` passes: a clone, keeping the original for
            // `session.set_control`.
            control.clone(),
        );
        let result = executor
            .execute(
                crate::personas::SWITCH_PERSONA_TOOL,
                serde_json::json!({ "id": "secretary" }),
                std::time::Duration::from_secs(1),
            )
            .await
            .expect("switching is a tool result, never a transport error");
        assert!(!result.is_error, "the switch was refused: {result:?}");

        let swap = control
            .take_prompt_swap()
            .expect("the session must be able to take the swap off its own handle");
        assert_eq!(swap.tag, "secretary");
        assert!(
            swap.prompt.contains("You are a secretary."),
            "the swapped prompt must be the new persona's: {}",
            swap.prompt
        );
    }

    #[tokio::test]
    async fn both_built_in_tools_are_declared_and_neither_shadows_the_other() {
        let mut book = a_book_whose_active_is_not_the_first();
        book.allow_agent_switch = true;

        let executor = persona_tools(
            time_tools(Arc::new(uia_core::tools::FakeExecutor::new()) as Arc<dyn ToolExecutor>),
            &book,
            None,
            uia_core::session::SessionControl::new(),
        );

        let names: Vec<String> = executor
            .list_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();

        assert_eq!(
            names,
            vec![
                crate::personas::SWITCH_PERSONA_TOOL.to_string(),
                crate::time::GET_CURRENT_TIME_TOOL.to_string(),
            ],
            "identity first, then the clock, then installed servers"
        );
    }

    #[tokio::test]
    async fn the_clock_is_callable_through_the_full_stack() {
        let book = a_book_whose_active_is_not_the_first();
        let executor = persona_tools(
            time_tools(Arc::new(uia_core::tools::FakeExecutor::new()) as Arc<dyn ToolExecutor>),
            &book,
            None,
            uia_core::session::SessionControl::new(),
        );
        let out = executor
            .execute(
                crate::time::GET_CURRENT_TIME_TOOL,
                serde_json::json!({"timezone": "Europe/London"}),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Europe/London"), "{}", out.content);
    }
}

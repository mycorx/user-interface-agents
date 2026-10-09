// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! In-app updates (docs/superpowers/specs/2026-10-07-auto-updater-design.md).
//!
//! Split like `activation_desktop`: everything here above the `desktop`
//! section has no `tauri` dependency and is unit tested in any environment;
//! the runtime that drives `tauri-plugin-updater` is gated behind the
//! `desktop` feature.

use std::time::Duration;

use serde::Serialize;
use uia_core::session::State;

/// Long enough that the first check never competes with the session
/// connecting at launch.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
pub const SIGNATURE_FAILURE: &str = "Update failed signature check";
/// How often a downloaded update re-reads the session state while it waits
/// for the assistant to go quiet before installing.
pub const INSTALL_GATE_POLL: Duration = Duration::from_millis(250);

/// Everything the HUD and Settings render about updates. Emitted on
/// [`crate::hud::UPDATE_EVENT`] and returned by `get_update_status`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate {
        current: String,
    },
    /// `error` is the last failed install of this version (a dropped
    /// download, a cancelled admin prompt): the offer stays so the user can
    /// retry.
    Available {
        version: String,
        notes: Option<String>,
        error: Option<String>,
    },
    Downloading {
        version: String,
        percent: Option<u8>,
    },
    Failed {
        message: String,
    },
}

pub enum CheckOutcome {
    NoUpdate,
    Found {
        version: String,
        notes: Option<String>,
    },
    Error(String),
}

/// Who asked for a check. A scheduled check runs unseen, so it must not
/// disturb an offer the user is already looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckTrigger {
    /// Settings' "Check now" or the tray item: the result, errors included,
    /// is what the user asked to see.
    Manual,
    /// The 30 s / 6 h background schedule.
    Scheduled,
}

pub enum InstallError {
    /// The download did not match the public key compiled into this build.
    Signature,
    Other(String),
}

pub fn next_check_delay(first: bool) -> Duration {
    if first {
        FIRST_CHECK_DELAY
    } else {
        CHECK_INTERVAL
    }
}

/// An install restarts the app, so it never interrupts the assistant mid-turn.
pub fn may_install(state: State) -> bool {
    matches!(state, State::Idle | State::Listening)
}

/// `rejected` is a version whose download already failed its signature check
/// in this run; it is not offered again.
pub fn status_after_check(
    outcome: CheckOutcome,
    current: &str,
    rejected: Option<&str>,
) -> UpdateStatus {
    match outcome {
        CheckOutcome::NoUpdate => UpdateStatus::UpToDate {
            current: current.to_string(),
        },
        CheckOutcome::Found { version, .. } if rejected == Some(version.as_str()) => {
            UpdateStatus::Failed {
                message: SIGNATURE_FAILURE.to_string(),
            }
        }
        CheckOutcome::Found { version, notes } => UpdateStatus::Available {
            version,
            notes,
            error: None,
        },
        CheckOutcome::Error(message) => UpdateStatus::Failed { message },
    }
}

/// Whether a check publishes `Checking` before it runs. A scheduled check
/// does not over an existing offer, so the banner never flickers.
pub fn shows_checking(previous: &UpdateStatus, trigger: CheckTrigger) -> bool {
    !(trigger == CheckTrigger::Scheduled && matches!(previous, UpdateStatus::Available { .. }))
}

/// [`status_after_check`], except that a scheduled check that errors leaves
/// an existing offer in place instead of replacing it with `Failed`.
pub fn status_after_triggered_check(
    previous: &UpdateStatus,
    trigger: CheckTrigger,
    outcome: CheckOutcome,
    current: &str,
    rejected: Option<&str>,
) -> UpdateStatus {
    if trigger == CheckTrigger::Scheduled
        && matches!(previous, UpdateStatus::Available { .. })
        && matches!(outcome, CheckOutcome::Error(_))
    {
        return previous.clone();
    }
    status_after_check(outcome, current, rejected)
}

/// An install that fails before its download starts (the re-check errors or
/// finds nothing). The offer on screen, if any, stays with the error on it;
/// otherwise the failure is shown as is.
pub fn status_after_early_install_error(previous: &UpdateStatus, message: String) -> UpdateStatus {
    match previous {
        UpdateStatus::Available { version, notes, .. } => {
            status_after_install_error(version, notes.clone(), InstallError::Other(message))
        }
        _ => UpdateStatus::Failed { message },
    }
}

pub fn status_after_install_error(
    version: &str,
    notes: Option<String>,
    error: InstallError,
) -> UpdateStatus {
    match error {
        InstallError::Signature => UpdateStatus::Failed {
            message: SIGNATURE_FAILURE.to_string(),
        },
        InstallError::Other(message) => UpdateStatus::Available {
            version: version.to_string(),
            notes,
            error: Some(message),
        },
    }
}

pub fn download_percent(downloaded: u64, total: Option<u64>) -> Option<u8> {
    match total {
        Some(total) if total > 0 => Some((downloaded.saturating_mul(100) / total).min(100) as u8),
        _ => None,
    }
}

#[cfg(feature = "desktop")]
pub use desktop::{UpdaterState, check_now, install, spawn_background_checks};

#[cfg(feature = "desktop")]
mod desktop {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri::{AppHandle, Emitter, Manager};
    use tauri_plugin_updater::UpdaterExt;

    /// Managed state: what the UI last saw, the session state the install
    /// gate reads, and a version this run will not offer again.
    #[derive(Default)]
    pub struct UpdaterState {
        status: Mutex<UpdateStatus>,
        session: Mutex<Option<State>>,
        rejected: Mutex<Option<String>>,
        busy: AtomicBool,
    }

    impl UpdaterState {
        pub fn status(&self) -> UpdateStatus {
            self.status.lock().unwrap().clone()
        }

        /// Called from `emit_state`, so the gate always sees what the HUD sees.
        pub fn set_session_state(&self, state: State) {
            *self.session.lock().unwrap() = Some(state);
        }

        fn session_state(&self) -> State {
            self.session.lock().unwrap().unwrap_or(State::Idle)
        }
    }

    fn publish(app: &AppHandle, status: UpdateStatus) {
        *app.state::<UpdaterState>().status.lock().unwrap() = status.clone();
        let _ = app.emit(crate::hud::UPDATE_EVENT, &status);
    }

    /// Holds `UpdaterState::busy` for one check or install and clears it on
    /// every exit path.
    struct BusyGuard<'a>(&'a AtomicBool);

    impl<'a> BusyGuard<'a> {
        fn acquire(busy: &'a AtomicBool) -> Option<Self> {
            (!busy.swap(true, Ordering::SeqCst)).then_some(Self(busy))
        }
    }

    impl Drop for BusyGuard<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    /// One check. A second call while a check or install runs returns the
    /// current status instead of starting another.
    pub async fn check_now(app: &AppHandle, trigger: CheckTrigger) -> UpdateStatus {
        let state = app.state::<UpdaterState>();
        let Some(_busy) = BusyGuard::acquire(&state.busy) else {
            return state.status();
        };
        let previous = state.status();
        if shows_checking(&previous, trigger) {
            publish(app, UpdateStatus::Checking);
        }
        let outcome = match app.updater() {
            Err(e) => CheckOutcome::Error(e.to_string()),
            Ok(updater) => match updater.check().await {
                Ok(Some(update)) => CheckOutcome::Found {
                    version: update.version,
                    notes: update.body,
                },
                Ok(None) => CheckOutcome::NoUpdate,
                Err(e) => CheckOutcome::Error(e.to_string()),
            },
        };
        let current = app.package_info().version.to_string();
        let rejected = state.rejected.lock().unwrap().clone();
        let status = status_after_triggered_check(
            &previous,
            trigger,
            outcome,
            &current,
            rejected.as_deref(),
        );
        publish(app, status.clone());
        status
    }

    /// Downloads, waits until the assistant is quiet, installs and restarts.
    /// Refused while the assistant is busy. On Windows the installer exits
    /// the app itself and this never returns.
    pub async fn install(app: &AppHandle) -> Result<(), String> {
        let state = app.state::<UpdaterState>();
        if !may_install(state.session_state()) {
            return Err("The assistant is busy. Install when it is quiet.".to_string());
        }
        let Some(_busy) = BusyGuard::acquire(&state.busy) else {
            return Err("An update check or install is already running.".to_string());
        };
        install_inner(app).await
    }

    async fn install_inner(app: &AppHandle) -> Result<(), String> {
        let found = match app.updater() {
            Err(e) => Err(e.to_string()),
            Ok(updater) => match updater.check().await {
                Ok(Some(update)) => Ok(update),
                Ok(None) => Err("No update is available any more.".to_string()),
                Err(e) => Err(e.to_string()),
            },
        };
        let update = match found {
            Ok(update) => update,
            Err(message) => {
                let previous = app.state::<UpdaterState>().status();
                publish(
                    app,
                    status_after_early_install_error(&previous, message.clone()),
                );
                return Err(message);
            }
        };
        let version = update.version.clone();
        let notes = update.body.clone();
        publish(
            app,
            UpdateStatus::Downloading {
                version: version.clone(),
                percent: Some(0),
            },
        );

        let progress_app = app.clone();
        let progress_version = version.clone();
        let mut downloaded: u64 = 0;
        let mut last: Option<u8> = Some(0);
        // `download` verifies the signature, so a bad one fails here.
        let bytes = match update
            .download(
                move |chunk, total| {
                    downloaded += chunk as u64;
                    let percent = download_percent(downloaded, total);
                    if percent != last {
                        last = percent;
                        publish(
                            &progress_app,
                            UpdateStatus::Downloading {
                                version: progress_version.clone(),
                                percent,
                            },
                        );
                    }
                },
                || {},
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => return Err(install_failed(app, &version, notes, e)),
        };

        // A turn may have started during the download. The install exits or
        // restarts the app (and on Linux raises an admin prompt), so it
        // waits for the assistant to go quiet again.
        publish(
            app,
            UpdateStatus::Downloading {
                version: version.clone(),
                percent: Some(100),
            },
        );
        while !may_install(app.state::<UpdaterState>().session_state()) {
            tokio::time::sleep(INSTALL_GATE_POLL).await;
        }

        match update.install(bytes) {
            Ok(()) => app.restart(),
            Err(e) => Err(install_failed(app, &version, notes, e)),
        }
    }

    /// Records a signature failure (R3), publishes the resulting status and
    /// returns the message for the command's `Err`.
    fn install_failed(
        app: &AppHandle,
        version: &str,
        notes: Option<String>,
        e: tauri_plugin_updater::Error,
    ) -> String {
        let kind = classify(&e);
        if matches!(kind, InstallError::Signature) {
            *app.state::<UpdaterState>().rejected.lock().unwrap() = Some(version.to_string());
        }
        publish(app, status_after_install_error(version, notes, kind));
        e.to_string()
    }

    fn classify(e: &tauri_plugin_updater::Error) -> InstallError {
        use tauri_plugin_updater::Error as E;
        match e {
            E::Minisign(_)
            | E::Base64(_)
            | E::SignatureUtf8(_)
            | E::SignedVersionMismatch { .. }
            | E::MissingSignedVersion => InstallError::Signature,
            other => InstallError::Other(other.to_string()),
        }
    }

    /// The background schedule. Re-reads the setting before every check, so
    /// turning it off or on needs no restart.
    pub fn spawn_background_checks(app: AppHandle, agent_settings_path: PathBuf) {
        tauri::async_runtime::spawn(async move {
            let mut first = true;
            loop {
                tokio::time::sleep(next_check_delay(first)).await;
                first = false;
                let settings: crate::settings::AgentSettings =
                    crate::settings::load(&agent_settings_path).unwrap_or_default();
                if crate::settings::resolve_auto_update_check(settings.auto_update_check) {
                    check_now(&app, CheckTrigger::Scheduled).await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_check_waits_for_startup_then_every_six_hours() {
        assert_eq!(next_check_delay(true), Duration::from_secs(30));
        assert_eq!(next_check_delay(false), Duration::from_secs(6 * 60 * 60));
    }

    #[test]
    fn installs_only_while_the_session_is_quiet() {
        for (state, allowed) in [
            (State::Idle, true),
            (State::Listening, true),
            (State::Connecting, false),
            (State::Thinking, false),
            (State::ToolRunning, false),
            (State::Speaking, false),
            (State::Interrupting, false),
        ] {
            assert_eq!(may_install(state), allowed, "{state:?}");
        }
    }

    #[test]
    fn a_check_maps_to_the_status_the_ui_renders() {
        assert_eq!(
            status_after_check(CheckOutcome::NoUpdate, "0.2.0", None),
            UpdateStatus::UpToDate {
                current: "0.2.0".into()
            }
        );
        assert_eq!(
            status_after_check(
                CheckOutcome::Found {
                    version: "0.3.0".into(),
                    notes: Some("n".into())
                },
                "0.2.0",
                None
            ),
            UpdateStatus::Available {
                version: "0.3.0".into(),
                notes: Some("n".into()),
                error: None
            }
        );
        assert_eq!(
            status_after_check(CheckOutcome::Error("offline".into()), "0.2.0", None),
            UpdateStatus::Failed {
                message: "offline".into()
            }
        );
    }

    #[test]
    fn a_version_that_failed_its_signature_is_not_offered_again() {
        assert_eq!(
            status_after_check(
                CheckOutcome::Found {
                    version: "0.3.0".into(),
                    notes: None
                },
                "0.2.0",
                Some("0.3.0")
            ),
            UpdateStatus::Failed {
                message: SIGNATURE_FAILURE.into()
            }
        );
        // A newer version than the rejected one is offered normally.
        assert!(matches!(
            status_after_check(
                CheckOutcome::Found {
                    version: "0.3.1".into(),
                    notes: None
                },
                "0.2.0",
                Some("0.3.0")
            ),
            UpdateStatus::Available { .. }
        ));
    }

    #[test]
    fn a_failed_install_keeps_the_offer_unless_the_signature_failed() {
        assert_eq!(
            status_after_install_error("0.3.0", None, InstallError::Other("cancelled".into())),
            UpdateStatus::Available {
                version: "0.3.0".into(),
                notes: None,
                error: Some("cancelled".into())
            }
        );
        assert_eq!(
            status_after_install_error("0.3.0", None, InstallError::Signature),
            UpdateStatus::Failed {
                message: SIGNATURE_FAILURE.into()
            }
        );
    }

    fn offer() -> UpdateStatus {
        UpdateStatus::Available {
            version: "0.3.0".into(),
            notes: Some("n".into()),
            error: None,
        }
    }

    #[test]
    fn a_scheduled_check_shows_no_checking_over_an_offer() {
        assert!(!shows_checking(&offer(), CheckTrigger::Scheduled));
        assert!(shows_checking(&offer(), CheckTrigger::Manual));
        assert!(shows_checking(&UpdateStatus::Idle, CheckTrigger::Scheduled));
        assert!(shows_checking(
            &UpdateStatus::UpToDate {
                current: "0.2.0".into()
            },
            CheckTrigger::Scheduled
        ));
    }

    #[test]
    fn a_failed_scheduled_check_keeps_an_existing_offer() {
        let error = || CheckOutcome::Error("offline".into());
        assert_eq!(
            status_after_triggered_check(&offer(), CheckTrigger::Scheduled, error(), "0.2.0", None),
            offer()
        );
        // A manual check shows its error, so Settings can say what went wrong.
        assert_eq!(
            status_after_triggered_check(&offer(), CheckTrigger::Manual, error(), "0.2.0", None),
            UpdateStatus::Failed {
                message: "offline".into()
            }
        );
        // With no offer on screen a scheduled failure is reported as before.
        assert_eq!(
            status_after_triggered_check(
                &UpdateStatus::Idle,
                CheckTrigger::Scheduled,
                error(),
                "0.2.0",
                None
            ),
            UpdateStatus::Failed {
                message: "offline".into()
            }
        );
        // A successful scheduled check still replaces the offer.
        assert_eq!(
            status_after_triggered_check(
                &offer(),
                CheckTrigger::Scheduled,
                CheckOutcome::Found {
                    version: "0.3.1".into(),
                    notes: None
                },
                "0.2.0",
                None
            ),
            UpdateStatus::Available {
                version: "0.3.1".into(),
                notes: None,
                error: None
            }
        );
    }

    #[test]
    fn an_install_that_fails_before_downloading_keeps_the_offer_with_its_error() {
        assert_eq!(
            status_after_early_install_error(&offer(), "gone".into()),
            UpdateStatus::Available {
                version: "0.3.0".into(),
                notes: Some("n".into()),
                error: Some("gone".into())
            }
        );
        assert_eq!(
            status_after_early_install_error(&UpdateStatus::Idle, "gone".into()),
            UpdateStatus::Failed {
                message: "gone".into()
            }
        );
    }

    #[test]
    fn download_percent_is_clamped_and_unknown_without_a_total() {
        assert_eq!(download_percent(50, Some(200)), Some(25));
        assert_eq!(download_percent(300, Some(200)), Some(100));
        assert_eq!(download_percent(10, None), None);
        assert_eq!(download_percent(10, Some(0)), None);
    }

    #[test]
    fn the_status_serialises_to_the_payload_the_frontend_reads() {
        let json = |s: &UpdateStatus| serde_json::to_value(s).unwrap();
        assert_eq!(
            json(&UpdateStatus::Idle),
            serde_json::json!({"status": "idle"})
        );
        assert_eq!(
            json(&UpdateStatus::UpToDate {
                current: "0.2.0".into()
            }),
            serde_json::json!({"status": "up_to_date", "current": "0.2.0"})
        );
        assert_eq!(
            json(&UpdateStatus::Available {
                version: "0.3.0".into(),
                notes: None,
                error: None
            }),
            serde_json::json!({"status": "available", "version": "0.3.0", "notes": null, "error": null})
        );
        assert_eq!(
            json(&UpdateStatus::Downloading {
                version: "0.3.0".into(),
                percent: Some(42)
            }),
            serde_json::json!({"status": "downloading", "version": "0.3.0", "percent": 42})
        );
        assert_eq!(
            json(&UpdateStatus::Failed {
                message: "x".into()
            }),
            serde_json::json!({"status": "failed", "message": "x"})
        );
    }
}

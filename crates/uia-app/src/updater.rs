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

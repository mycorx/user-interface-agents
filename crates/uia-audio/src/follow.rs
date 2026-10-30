// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Decisions shared by every backend that follows the OS default device or
//! waits for a pinned one.
//!
//! Pure functions and plain enums, compiled on every OS, so the policy is
//! tested in the Linux CI that cannot build the WASAPI or CoreAudio code.
//! The backends only map their OS types onto these.

use uia_core::audio::{AudioError, WaitingFor};

/// Why a device could not be opened.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
#[derive(Debug)]
pub(crate) enum OpenFailure {
    /// The user pinned `name` and no present device matches it.
    MissingPinned { name: String, detail: String },
    /// Anything else: no default device at all, an OS error.
    Other(AudioError),
}

impl From<OpenFailure> for AudioError {
    fn from(failure: OpenFailure) -> Self {
        match failure {
            OpenFailure::MissingPinned { detail, .. } => AudioError::DeviceUnavailable(detail),
            OpenFailure::Other(e) => e,
        }
    }
}

/// What to do when the very first open fails.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FirstOpen {
    /// Start anyway and wait for this.
    Wait(WaitingFor),
    /// Report the error, as before.
    Fail,
}

/// A missing pinned name is waited for; every other failure keeps today's
/// handling and fails the open.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
pub(crate) fn first_open(failure: &OpenFailure) -> FirstOpen {
    match failure {
        OpenFailure::MissingPinned { name, .. } => FirstOpen::Wait(WaitingFor::Named(name.clone())),
        OpenFailure::Other(_) => FirstOpen::Fail,
    }
}

/// What a direction with this configured device is waiting for.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
pub(crate) fn waiting_for(pinned: Option<&str>) -> WaitingFor {
    match pinned {
        Some(name) => WaitingFor::Named(name.to_string()),
        None => WaitingFor::Default,
    }
}

/// Which way audio flows.
// Only the cpal legs consume this so far.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Flow {
    Capture,
    Render,
}

/// A Windows device role, as the default-changed notification reports it.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the WASAPI default-change notification has not landed yet"
    )
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    Console,
    Multimedia,
    Communications,
}

/// Should a default-device change make this direction reopen?
///
/// Only an unpinned direction follows, only for its own flow, and only for
/// the communications role: that is the one UIA opens, so the other two
/// would reopen onto the device it already has.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the WASAPI default-change notification has not landed yet"
    )
)]
pub(crate) fn reopen_for_default_change(
    pinned: bool,
    ours: Flow,
    changed: Flow,
    role: Role,
) -> bool {
    !pinned && ours == changed && role == Role::Communications
}

/// Does this cpal host move a default-device stream to the new default by
/// itself, so UIA need not reopen it? `host_name` is `HostId::name()`.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
pub(crate) fn host_follows_default(host_name: &str, flow: Flow) -> bool {
    match host_name {
        "PipeWire" => true,
        "CoreAudio" => flow == Flow::Render,
        _ => false,
    }
}

/// Is the playback queue still valid after a reopen? Samples queued for one
/// rate would play at the wrong pitch on another.
#[cfg_attr(not(feature = "cpal-io"), allow(dead_code))]
pub(crate) fn keep_queue_on_reopen(old_rate_hz: u32, new_rate_hz: u32) -> bool {
    old_rate_hz == new_rate_hz
}

/// How looking up one direction's device went.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the split echo-cancelling unit has not landed yet"
    )
)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    Found,
    MissingPinned(String),
    Failed,
}

/// How one direction runs when the unit is split.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the split echo-cancelling unit has not landed yet"
    )
)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LegPlan {
    /// Run without echo cancellation.
    Plain,
    /// Not present yet: wait for this pinned name.
    Wait(String),
}

/// What the echo-cancelling unit does at open.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the split echo-cancelling unit has not landed yet"
    )
)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UnitPlan {
    /// Both devices are there: open the unit.
    Open,
    /// A pinned device is missing: run each leg on its own.
    Split { input: LegPlan, output: LegPlan },
    /// An OS failure on either side: today's handling.
    Fail,
}

/// Decide the unit from how each direction resolved.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the split echo-cancelling unit has not landed yet"
    )
)]
pub(crate) fn plan_unit(input: &Resolution, output: &Resolution) -> UnitPlan {
    fn leg(r: &Resolution) -> Option<LegPlan> {
        match r {
            Resolution::Found => Some(LegPlan::Plain),
            Resolution::MissingPinned(name) => Some(LegPlan::Wait(name.clone())),
            Resolution::Failed => None,
        }
    }
    match (leg(input), leg(output)) {
        (Some(LegPlan::Plain), Some(LegPlan::Plain)) => UnitPlan::Open,
        (Some(input), Some(output)) => UnitPlan::Split { input, output },
        _ => UnitPlan::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing(name: &str) -> OpenFailure {
        OpenFailure::MissingPinned {
            name: name.into(),
            detail: format!("no input device matching {name}"),
        }
    }

    #[test]
    fn a_missing_pinned_name_waits_at_first_open() {
        assert_eq!(
            first_open(&missing("Jabra")),
            FirstOpen::Wait(WaitingFor::Named("Jabra".into()))
        );
        let err: AudioError = missing("Jabra").into();
        assert_eq!(
            err.to_string(),
            "audio device unavailable: no input device matching Jabra"
        );
    }

    #[test]
    fn any_other_first_open_failure_fails() {
        let other = OpenFailure::Other(AudioError::DeviceUnavailable("no default".into()));
        assert_eq!(first_open(&other), FirstOpen::Fail);
    }

    #[test]
    fn a_pinned_direction_waits_for_its_name() {
        assert_eq!(
            waiting_for(Some("Jabra")),
            WaitingFor::Named("Jabra".into())
        );
    }

    #[test]
    fn an_unpinned_direction_waits_for_a_default_not_a_name() {
        assert_eq!(waiting_for(None), WaitingFor::Default);
    }

    #[test]
    fn an_unpinned_direction_reopens_on_its_own_flows_communications_default_change() {
        for flow in [Flow::Capture, Flow::Render] {
            assert!(reopen_for_default_change(
                false,
                flow,
                flow,
                Role::Communications
            ));
        }
    }

    #[test]
    fn console_and_multimedia_default_changes_are_ignored() {
        for role in [Role::Console, Role::Multimedia] {
            assert!(!reopen_for_default_change(
                false,
                Flow::Capture,
                Flow::Capture,
                role
            ));
        }
    }

    #[test]
    fn the_other_flows_default_change_is_ignored() {
        assert!(!reopen_for_default_change(
            false,
            Flow::Capture,
            Flow::Render,
            Role::Communications
        ));
        assert!(!reopen_for_default_change(
            false,
            Flow::Render,
            Flow::Capture,
            Role::Communications
        ));
    }

    #[test]
    fn a_pinned_direction_ignores_every_default_change() {
        for role in [Role::Console, Role::Multimedia, Role::Communications] {
            assert!(!reopen_for_default_change(
                true,
                Flow::Render,
                Flow::Render,
                role
            ));
        }
    }

    #[test]
    fn pipewire_follows_the_default_in_both_directions() {
        assert!(host_follows_default("PipeWire", Flow::Capture));
        assert!(host_follows_default("PipeWire", Flow::Render));
    }

    #[test]
    fn coreaudio_follows_the_default_output_only() {
        assert!(host_follows_default("CoreAudio", Flow::Render));
        assert!(!host_follows_default("CoreAudio", Flow::Capture));
    }

    #[test]
    fn alsa_and_wasapi_follow_nothing_by_themselves() {
        for host in ["ALSA", "WASAPI"] {
            assert!(!host_follows_default(host, Flow::Capture));
            assert!(!host_follows_default(host, Flow::Render));
        }
    }

    #[test]
    fn the_playback_queue_survives_a_reopen_at_the_same_rate() {
        assert!(keep_queue_on_reopen(48_000, 48_000));
    }

    #[test]
    fn the_playback_queue_is_dropped_when_the_rate_changes() {
        assert!(!keep_queue_on_reopen(48_000, 44_100));
    }

    #[test]
    fn both_directions_found_opens_the_unit() {
        assert_eq!(
            plan_unit(&Resolution::Found, &Resolution::Found),
            UnitPlan::Open
        );
    }

    #[test]
    fn a_missing_pinned_input_runs_the_output_plain_and_waits_for_the_input() {
        assert_eq!(
            plan_unit(
                &Resolution::MissingPinned("Jabra".into()),
                &Resolution::Found
            ),
            UnitPlan::Split {
                input: LegPlan::Wait("Jabra".into()),
                output: LegPlan::Plain
            }
        );
    }

    #[test]
    fn both_pinned_and_missing_wait_for_both() {
        assert_eq!(
            plan_unit(
                &Resolution::MissingPinned("Jabra".into()),
                &Resolution::MissingPinned("Pebble".into())
            ),
            UnitPlan::Split {
                input: LegPlan::Wait("Jabra".into()),
                output: LegPlan::Wait("Pebble".into())
            }
        );
    }

    #[test]
    fn an_os_failure_on_either_side_fails() {
        assert_eq!(
            plan_unit(&Resolution::Failed, &Resolution::Found),
            UnitPlan::Fail
        );
        assert_eq!(
            plan_unit(&Resolution::Found, &Resolution::Failed),
            UnitPlan::Fail
        );
        assert_eq!(
            plan_unit(
                &Resolution::MissingPinned("Jabra".into()),
                &Resolution::Failed
            ),
            UnitPlan::Fail
        );
    }
}

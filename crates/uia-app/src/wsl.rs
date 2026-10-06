// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

// uia is a GUI-only client: an always-on-top transparent HUD, a tray icon, a
// global hotkey, and a microphone. WSL has none of those in any dependable
// form — WSLg gives a GTK surface but no tray, no system-wide hotkey, and no
// audio capture device — so a Linux build launched from a WSL shell fails
// deep inside GTK or cpal with an error that names neither WSL nor the real
// problem. This gate turns that into one sentence, up front.
//
// Everything here is a pure function over `WslEvidence` so it can be tested
// in the WSL2 development sandbox itself, where `--features desktop` cannot
// even be compiled (see `main.rs`'s header). `from_environment` is the only
// impure part, and it is the part with no decisions in it.

/// The observable facts that distinguish WSL from a native Linux desktop.
///
/// Borrowed rather than owned so the tests can build one from string
/// literals without ceremony.
#[derive(Debug, Clone, Copy, Default)]
pub struct WslEvidence<'a> {
    /// `/proc/sys/kernel/osrelease`, or `None` if it could not be read.
    pub osrelease: Option<&'a str>,
    /// `$WSL_DISTRO_NAME`, set by WSL itself in every distro shell.
    pub wsl_distro_name: Option<&'a str>,
    /// `$WSL_INTEROP`, set when the Windows interop socket is available.
    pub wsl_interop: Option<&'a str>,
    /// `$UIA_ALLOW_WSL`, the deliberate escape hatch.
    pub allow_override: Option<&'a str>,
}

/// The owned counterpart to [`WslEvidence`], holding what was actually read
/// from the host so the borrowed view above has something to point at.
///
/// Split in two deliberately: `from_environment` performs I/O and makes no
/// decisions, while every decision is a pure function of the borrowed view.
/// That is what lets the gate be tested in a sandbox whose own answer to
/// "are we on WSL?" is fixed.
#[derive(Debug, Clone, Default)]
pub struct WslFacts {
    pub osrelease: Option<String>,
    pub wsl_distro_name: Option<String>,
    pub wsl_interop: Option<String>,
    pub allow_override: Option<String>,
}

impl WslFacts {
    pub fn from_environment() -> Self {
        Self {
            // A read failure is `None`, not an error: an unreadable `/proc`
            // is one absent signal, and the environment variables below still
            // get their say.
            osrelease: std::fs::read_to_string("/proc/sys/kernel/osrelease").ok(),
            wsl_distro_name: std::env::var("WSL_DISTRO_NAME").ok(),
            wsl_interop: std::env::var("WSL_INTEROP").ok(),
            allow_override: std::env::var("UIA_ALLOW_WSL").ok(),
        }
    }

    pub fn evidence(&self) -> WslEvidence<'_> {
        WslEvidence {
            osrelease: self.osrelease.as_deref(),
            wsl_distro_name: self.wsl_distro_name.as_deref(),
            wsl_interop: self.wsl_interop.as_deref(),
            allow_override: self.allow_override.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WslDecision {
    Proceed,
    Block,
}

pub const WSL_BLOCK_MESSAGE: &str = concat!(
    "uia will not start under WSL.\n\n",
    "It is a GUI application that needs a real desktop session: a tray icon, ",
    "a system-wide hotkey, and a microphone. WSL provides none of these ",
    "reliably, so the app would fail later with a much less useful error.\n\n",
    "Run uia on Windows directly (the MSI installer), or on a native Linux ",
    "desktop (the .deb).\n\n",
    "To override this check anyway, set UIA_ALLOW_WSL=1.",
);

/// Whether the host looks like WSL, ignoring the override entirely.
///
/// Any single signal is sufficient: `/proc` can be unreadable and the
/// environment can be scrubbed, but it is vanishingly unlikely that a real
/// WSL session hides all of them at once.
fn looks_like_wsl(evidence: &WslEvidence<'_>) -> bool {
    let kernel_says_so = evidence
        .osrelease
        // WSL1 reports `...-Microsoft`, WSL2 `...-microsoft-standard-WSL2`;
        // lowercasing covers both without two separate patterns.
        .is_some_and(|release| release.to_ascii_lowercase().contains("microsoft"));

    kernel_says_so || is_set(evidence.wsl_distro_name) || is_set(evidence.wsl_interop)
}

/// A variable counts as set only if it has a non-blank value. `WSL_INTEROP=`
/// is what an over-eager profile script leaves behind, not what WSL sets.
fn is_set(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

/// Whether `$UIA_ALLOW_WSL` was set deliberately to something affirmative.
///
/// Mere presence is not enough: `export UIA_ALLOW_WSL=$UNSET` yields an empty
/// string, and treating that as consent would disable the gate for people who
/// never asked.
fn override_is_affirmative(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn decide(evidence: &WslEvidence<'_>) -> WslDecision {
    if looks_like_wsl(evidence) && !override_is_affirmative(evidence.allow_override) {
        WslDecision::Block
    } else {
        WslDecision::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real contents of `/proc/sys/kernel/osrelease` under WSL2.
    const WSL2_OSRELEASE: &str = "6.6.87.2-microsoft-standard-WSL2";
    /// WSL1 spelled it with a capital M and no `WSL` marker at all, which is
    /// why the check below is case-insensitive rather than a `contains("WSL")`.
    const WSL1_OSRELEASE: &str = "4.4.0-19041-Microsoft";
    const NATIVE_OSRELEASE: &str = "6.11.0-19-generic";

    fn native() -> WslEvidence<'static> {
        WslEvidence {
            osrelease: Some(NATIVE_OSRELEASE),
            wsl_distro_name: None,
            wsl_interop: None,
            allow_override: None,
        }
    }

    #[test]
    fn a_wsl2_kernel_release_is_blocked() {
        let evidence = WslEvidence {
            osrelease: Some(WSL2_OSRELEASE),
            ..native()
        };
        assert_eq!(decide(&evidence), WslDecision::Block);
    }

    #[test]
    fn a_wsl1_kernel_release_is_blocked_despite_its_different_capitalisation() {
        let evidence = WslEvidence {
            osrelease: Some(WSL1_OSRELEASE),
            ..native()
        };
        assert_eq!(decide(&evidence), WslDecision::Block);
    }

    #[test]
    fn a_native_linux_kernel_release_proceeds() {
        assert_eq!(decide(&native()), WslDecision::Proceed);
    }

    #[test]
    fn wsl_distro_name_alone_is_enough_to_block() {
        // `/proc` can be unreadable (a hardened container, a stripped mount
        // namespace), so the environment has to stand on its own rather than
        // only corroborating the kernel string.
        let evidence = WslEvidence {
            osrelease: None,
            wsl_distro_name: Some("Ubuntu-24.04"),
            ..native()
        };
        assert_eq!(decide(&evidence), WslDecision::Block);
    }

    #[test]
    fn wsl_interop_alone_is_enough_to_block() {
        let evidence = WslEvidence {
            osrelease: None,
            wsl_interop: Some("/run/WSL/8_interop"),
            ..native()
        };
        assert_eq!(decide(&evidence), WslDecision::Block);
    }

    #[test]
    fn no_evidence_at_all_proceeds_rather_than_blocking_a_native_machine() {
        // Failing OPEN is deliberate. A false positive bricks the app on a
        // real Linux desktop for someone with no way past it; a false
        // negative merely lets a WSL user reach the GTK error they would
        // have got anyway.
        let evidence = WslEvidence {
            osrelease: None,
            wsl_distro_name: None,
            wsl_interop: None,
            allow_override: None,
        };
        assert_eq!(decide(&evidence), WslDecision::Proceed);
    }

    #[test]
    fn the_override_lets_a_wsl_user_through() {
        let evidence = WslEvidence {
            osrelease: Some(WSL2_OSRELEASE),
            allow_override: Some("1"),
            ..native()
        };
        assert_eq!(decide(&evidence), WslDecision::Proceed);
    }

    #[test]
    fn the_override_must_be_set_deliberately_not_merely_present() {
        // An empty or "0" value is what an unset variable looks like when a
        // shell script exports it unconditionally (`export UIA_ALLOW_WSL=$X`
        // with `$X` unset). Honouring that would silently disable the gate.
        for value in ["", "0", "false", "no"] {
            let evidence = WslEvidence {
                osrelease: Some(WSL2_OSRELEASE),
                allow_override: Some(value),
                ..native()
            };
            assert_eq!(
                decide(&evidence),
                WslDecision::Block,
                "UIA_ALLOW_WSL={value:?} should not disable the gate"
            );
        }
    }

    #[test]
    fn the_override_accepts_the_spellings_a_person_would_actually_type() {
        for value in ["1", "true", "TRUE", "yes"] {
            let evidence = WslEvidence {
                osrelease: Some(WSL2_OSRELEASE),
                allow_override: Some(value),
                ..native()
            };
            assert_eq!(
                decide(&evidence),
                WslDecision::Proceed,
                "UIA_ALLOW_WSL={value:?} should disable the gate"
            );
        }
    }

    #[test]
    fn the_override_does_not_invent_a_reason_to_block_a_native_machine() {
        // The override only ever suppresses a block; it is not itself a
        // signal about the host.
        let evidence = WslEvidence {
            allow_override: Some("1"),
            ..native()
        };
        assert_eq!(decide(&evidence), WslDecision::Proceed);
    }

    #[test]
    fn an_empty_wsl_variable_is_not_evidence_of_wsl() {
        // `export WSL_DISTRO_NAME=` in a stray profile script sets the
        // variable to an empty string. Reading that as "we are on WSL" would
        // block a native desktop on the strength of a typo.
        let evidence = WslEvidence {
            osrelease: Some(NATIVE_OSRELEASE),
            wsl_distro_name: Some(""),
            wsl_interop: Some("   "),
            allow_override: None,
        };
        assert_eq!(decide(&evidence), WslDecision::Proceed);
    }

    #[test]
    fn facts_carry_every_field_through_to_the_evidence() {
        // A field dropped in this mapping silently disables one leg of the
        // gate, which no other test would notice.
        let facts = WslFacts {
            osrelease: Some(WSL2_OSRELEASE.to_string()),
            wsl_distro_name: Some("Ubuntu-24.04".to_string()),
            wsl_interop: Some("/run/WSL/8_interop".to_string()),
            allow_override: Some("1".to_string()),
        };
        let evidence = facts.evidence();

        assert_eq!(evidence.osrelease, Some(WSL2_OSRELEASE));
        assert_eq!(evidence.wsl_distro_name, Some("Ubuntu-24.04"));
        assert_eq!(evidence.wsl_interop, Some("/run/WSL/8_interop"));
        assert_eq!(evidence.allow_override, Some("1"));
    }

    #[test]
    fn the_block_message_names_the_override_so_the_user_is_not_stuck() {
        assert!(
            WSL_BLOCK_MESSAGE.contains("UIA_ALLOW_WSL"),
            "a gate with no documented way past it is a dead end: {WSL_BLOCK_MESSAGE}"
        );
    }
}

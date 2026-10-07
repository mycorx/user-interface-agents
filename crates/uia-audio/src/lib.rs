// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

#[cfg(feature = "aec")]
pub mod aec;
mod backoff;
pub mod codec;
// Gated together: these two are the only modules that touch cpal, and the
// `cpal-io` feature exists purely to keep them (and cpal) out of
// `uia-openai`'s dependency graph.
#[cfg(feature = "cpal-io")]
pub mod input;
#[cfg(feature = "cpal-io")]
pub mod output;
// Double-guarded on purpose: `windows` is a `cfg(windows)`-only dependency, so
// a bare `feature = "wasapi-aec"` guard would not compile anywhere else.
#[cfg(all(windows, feature = "wasapi-aec"))]
pub mod wasapi;
// Double-guarded like `wasapi`: the objc2 audio crates are macOS-only
// dependencies, so a bare feature guard would not compile anywhere else.
#[cfg(all(target_os = "macos", feature = "coreaudio-aec"))]
pub mod coreaudio;

/// Does `device_name` satisfy a user's configured `wanted` name?
///
/// Case-insensitive substring, so a user can paste the name Windows Sound
/// settings shows without matching its punctuation exactly -- "Webcam C920"
/// finds "Microphone (HD Pro Webcam C920)".
///
/// This lives here, ungated, because four device lookups
/// (`input::find_input_device_by_name`, `output::find_output_device_by_name`,
/// `wasapi::com::find_device_by_name`, `coreaudio::device::choose`) and
/// `uia-app`'s startup filter all have
/// to agree on it. They previously each spelled it out. A filter that decided
/// "this persisted name matches nothing" using a *stricter* rule than the
/// lookup would discard settings that in fact work -- dropping "Webcam" as
/// unavailable while the lookup would have found it -- so the rule gets one
/// definition rather than five copies that can drift apart.
pub fn device_name_matches(device_name: &str, wanted: &str) -> bool {
    device_name.to_lowercase().contains(&wanted.to_lowercase())
}

/// `names` with repeats removed, keeping each name's first position.
///
/// Every device list a settings UI gets goes through this. Devices are
/// selected by name (see [`device_name_matches`]), so a second device with an
/// identical name could never be selected anyway, and the Settings UI keys
/// its options by name — a repeat makes it throw and show no devices at all.
#[cfg(any(
    feature = "cpal-io",
    all(target_os = "macos", feature = "coreaudio-aec")
))]
pub(crate) fn dedupe_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

#[cfg(test)]
mod device_name_tests {
    use super::device_name_matches;

    #[test]
    fn matching_is_case_insensitive_and_by_substring() {
        assert!(device_name_matches(
            "Microphone (HD Pro Webcam C920)",
            "webcam c920"
        ));
        assert!(device_name_matches(
            "Speakers (Creative Pebble X)",
            "Creative"
        ));
        assert!(device_name_matches("Same", "same"));
    }

    /// The property the startup filter depends on: a partial name the lookup
    /// would accept must not be judged "unavailable" and thrown away.
    #[test]
    fn a_partial_name_the_lookup_accepts_is_not_a_miss() {
        let available = "Microphone (HD Pro Webcam C920)";
        assert!(device_name_matches(available, "Webcam"));
        assert_ne!(available, "Webcam", "equality would have rejected this");
    }

    #[test]
    fn an_unrelated_name_does_not_match() {
        assert!(!device_name_matches("Some Other Mic", "HD Pro Webcam C920"));
    }
}

#[cfg(all(
    test,
    any(
        feature = "cpal-io",
        all(target_os = "macos", feature = "coreaudio-aec")
    )
))]
mod dedupe_tests {
    use super::dedupe_names;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn duplicates_are_dropped_keeping_first_seen_order() {
        assert_eq!(
            dedupe_names(names(&[
                "Creative Pebble X",
                "Jabra",
                "Creative Pebble X",
                "C920",
                "Jabra",
            ])),
            names(&["Creative Pebble X", "Jabra", "C920"])
        );
    }

    #[test]
    fn unique_and_empty_lists_are_unchanged() {
        assert_eq!(dedupe_names(names(&["B", "A"])), names(&["B", "A"]));
        assert_eq!(dedupe_names(Vec::new()), Vec::<String>::new());
    }
}

// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The clock behind `get_current_time`: formatting and zone resolution.
//!
//! Split from `time_executor.rs` the way `personas.rs` is split from
//! `persona_executor.rs` — the logic here is pure and synchronous, and the
//! decorator there is the only part that has to know about `ToolExecutor`.

use chrono::{DateTime, Utc};
use chrono_tz::{OffsetName, TZ_VARIANTS, Tz};
use uia_core::tools::ToolDescriptor;

/// One line, because the model reading this has to speak it. A JSON object
/// invites it to read field names aloud; a multi-line block invites it to
/// narrate the structure.
pub fn render_time(now: DateTime<Utc>, tz: Tz) -> String {
    let t = now.with_timezone(&tz);
    // `abbreviation()` is `None` for zones whose short form is just a numeric
    // offset (Asia/Kathmandu, Pacific/Chatham). Dropping the field is right;
    // a placeholder would be spoken.
    let located = match t.offset().abbreviation() {
        Some(abbr) => format!("{}, UTC{}, {}", tz.name(), t.format("%:z"), abbr),
        None => format!("{}, UTC{}", tz.name(), t.format("%:z")),
    };
    format!(
        "{} — {}, {} ({})",
        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        t.format("%A, %-d %B %Y"),
        t.format("%-I:%M %P"),
        located,
    )
}

/// `Err` is text for the model, not for a log: the tool ran fine and the
/// argument was wrong, so the caller turns this into an error-flagged
/// `ToolResult` the model can read and retry from.
pub fn resolve_zone(requested: Option<&str>, local: Tz) -> Result<Tz, String> {
    let Some(name) = requested else {
        return Ok(local);
    };
    if let Ok(tz) = name.parse::<Tz>() {
        return Ok(tz);
    }
    // 597 variants, walked only on the miss path. Case is the one difference
    // worth absorbing: it cannot change which place is meant.
    if let Some(tz) = TZ_VARIANTS
        .iter()
        .find(|tz| tz.name().eq_ignore_ascii_case(name))
    {
        return Ok(*tz);
    }
    Err(format!(
        "unknown timezone {name:?} — expected an IANA identifier of the form \
         Area/Location, such as America/Los_Angeles"
    ))
}

/// The tool's name on the wire. Bare, with no namespace: every MCP tool is
/// `server.tool` and so contains a dot, which is what keeps a dotless
/// built-in name unique. It also already satisfies the `^[a-zA-Z0-9_-]+$`
/// both OpenAI and Nova enforce, so `wire_tool_name` has nothing to rewrite.
pub const GET_CURRENT_TIME_TOOL: &str = "get_current_time";

/// Injected rather than called inline, because a tool that reads the real
/// clock cannot be asserted against — and the DST cases would otherwise only
/// be testable twice a year.
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> DateTime<Utc>;
}

/// Split from `Clock` because the device's zone and the device's instant
/// come from different places and fail differently.
pub trait LocalZone: Send + Sync {
    fn local(&self) -> Tz;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub struct SystemLocalZone;

impl LocalZone for SystemLocalZone {
    /// UTC rather than a panic when the host cannot say: a wrong-by-an-offset
    /// answer that names the zone it used is recoverable, and the user can
    /// pass a zone explicitly. Refusing to tell the time at all is not.
    fn local(&self) -> Tz {
        iana_time_zone::get_timezone()
            .ok()
            .and_then(|name| name.parse().ok())
            .unwrap_or(Tz::UTC)
    }
}

/// Test doubles. Public, not `#[cfg(test)]`: `time_executor.rs` is a
/// different module and needs them, exactly as it needs `FakeExecutor`.
pub struct FixedClock(pub DateTime<Utc>);

impl Clock for FixedClock {
    fn now_utc(&self) -> DateTime<Utc> {
        self.0
    }
}

pub struct FixedZone(pub Tz);

impl LocalZone for FixedZone {
    fn local(&self) -> Tz {
        self.0
    }
}

/// One tool, not three. `get_current_date` and `get_day_of_week` would be
/// extra routing decisions for the model to get wrong, and all three are one
/// clock reading — so date and weekday ride in the same result.
pub fn get_current_time_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: GET_CURRENT_TIME_TOOL.into(),
        description: "Get the current date, day of the week, and time. \
                      Optionally for a specific place, given as an IANA \
                      timezone identifier such as Europe/London or \
                      Australia/Melbourne. Omit the timezone to get the \
                      user's own local time."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone identifier, e.g. \"Europe/London\". Omit for the device's own timezone."
                }
            }
        }),
        requires_confirmation: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> DateTime<Utc> {
        iso.parse().expect("test timestamp")
    }
    fn zone(name: &str) -> Tz {
        name.parse().expect("test zone")
    }

    #[test]
    fn renders_iso_then_human_date_time_and_zone_on_one_line() {
        let out = render_time(at("2026-09-07T13:10:04Z"), zone("Australia/Melbourne"));
        assert_eq!(
            out,
            "2026-09-07T23:10:04+10:00 — Monday, 7 September 2026, 11:10 pm \
             (Australia/Melbourne, UTC+10:00, AEST)"
        );
        assert!(!out.contains('\n'), "must be one line: {out}");
    }

    #[test]
    fn a_dst_transition_moves_the_offset_and_the_abbreviation() {
        // Melbourne's 2026 DST start: 02:00 local becomes 03:00 on 4 October,
        // which is 16:00 UTC on 3 October.
        let tz = zone("Australia/Melbourne");
        assert!(render_time(at("2026-10-03T15:30:00Z"), tz).contains("UTC+10:00, AEST"));
        assert!(render_time(at("2026-10-03T16:00:00Z"), tz).contains("UTC+11:00, AEDT"));
    }

    #[test]
    fn a_northern_hemisphere_zone_is_not_assumed_to_match_the_southern_one() {
        let tz = zone("Europe/London");
        assert!(render_time(at("2026-01-15T09:00:00Z"), tz).contains("UTC+00:00, GMT"));
        assert!(render_time(at("2026-09-07T13:10:04Z"), tz).contains("BST"));
    }

    #[test]
    fn fractional_hour_offsets_render_with_their_minutes() {
        let now = at("2026-09-07T13:10:04Z");
        assert!(render_time(now, zone("Asia/Kolkata")).contains("UTC+05:30, IST"));
        assert!(render_time(now, zone("Asia/Kathmandu")).contains("UTC+05:45"));
    }

    #[test]
    fn a_zone_with_no_abbreviation_omits_it_rather_than_printing_a_placeholder() {
        // chrono-tz returns None for zones whose abbreviation is just a
        // numeric offset. Printing "?" or "None" would be read aloud.
        let out = render_time(at("2026-09-07T13:10:04Z"), zone("Asia/Kathmandu"));
        assert!(out.ends_with("(Asia/Kathmandu, UTC+05:45)"), "{out}");
    }

    #[test]
    fn no_argument_means_the_devices_own_zone() {
        let local = zone("Australia/Melbourne");
        assert_eq!(resolve_zone(None, local).unwrap(), local);
    }

    #[test]
    fn a_named_zone_overrides_the_local_one() {
        let got = resolve_zone(Some("Europe/London"), zone("Australia/Melbourne")).unwrap();
        assert_eq!(got.name(), "Europe/London");
    }

    #[test]
    fn casing_is_normalised_rather_than_rejected() {
        // Parsing is case-sensitive, and a model that emits
        // "australia/melbourne" is not wrong about the place. Unlike an
        // abbreviation, the canonical spelling is unambiguous, so this is
        // normalisation and not guessing.
        let local = zone("UTC");
        assert_eq!(
            resolve_zone(Some("australia/melbourne"), local)
                .unwrap()
                .name(),
            "Australia/Melbourne"
        );
        assert_eq!(
            resolve_zone(Some("ASIA/TOKYO"), local).unwrap().name(),
            "Asia/Tokyo"
        );
    }

    #[test]
    fn an_abbreviation_is_refused_with_a_message_that_teaches_the_format() {
        // "PST" is deliberately NOT mapped: it is ambiguous about daylight
        // saving in a way an IANA name is not, so a guess here silently
        // mislocates someone.
        let err = resolve_zone(Some("PST"), zone("UTC")).unwrap_err();
        assert!(err.contains("PST"), "must quote the input: {err}");
        assert!(err.contains("Area/Location"), "must teach the form: {err}");
        assert!(
            err.contains("America/Los_Angeles"),
            "must give an example: {err}"
        );
    }

    #[test]
    fn an_unknown_zone_is_refused_and_quoted() {
        let err = resolve_zone(Some("Nowhere/Nope"), zone("UTC")).unwrap_err();
        assert!(err.contains("Nowhere/Nope"), "{err}");
    }

    #[test]
    fn a_fixed_clock_returns_what_it_was_given() {
        let when = at("2026-09-07T13:10:04Z");
        assert_eq!(FixedClock(when).now_utc(), when);
    }

    #[test]
    fn the_system_zone_resolves_to_a_real_zone() {
        // Whatever the host is set to, it must be a zone we can name — the
        // fallback is UTC, never a panic.
        let name = SystemLocalZone.local().name();
        assert!(name.parse::<Tz>().is_ok(), "not a real zone: {name}");
    }

    #[test]
    fn the_descriptor_names_the_tool_and_takes_an_optional_timezone() {
        let d = get_current_time_descriptor();
        assert_eq!(d.name, "get_current_time");
        assert!(!d.requires_confirmation);
        let props = &d.input_schema["properties"];
        assert!(props["timezone"].is_object(), "timezone property missing");
        // Optional: nothing is required, so the model can just ask for "now".
        assert!(
            d.input_schema.get("required").is_none(),
            "timezone must not be required"
        );
        assert!(
            d.description.contains("IANA"),
            "the description is what teaches the model the format"
        );
    }
}

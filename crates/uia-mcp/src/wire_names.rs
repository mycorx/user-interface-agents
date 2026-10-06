// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Tool names as they go on the wire, and back.
//!
//! **Both** providers enforce `^[a-zA-Z0-9_-]+$` on tool names and reject the
//! request when one violates it — OpenAI by refusing the whole `session.update`
//! (S9, live), and Nova with a 400 `ValidationException` that fails the entire
//! session before it starts (S14, live):
//!
//! ```text
//! Malformed input request: #/toolConfig/tools/0/toolSpec/name:
//! string [clock.now] does not match pattern ^[a-zA-Z0-9_-]+$
//! ```
//!
//! S8's `namespaced()` builds `server.tool`, so every namespaced tool is
//! refused by both until the dot is translated away. This lives in `uia-mcp`
//! rather than in either engine because it is a property of the names MCP
//! produces, not of one provider — and because a second copy is what drifts.

use uia_core::tools::ToolDescriptor;

/// `.` becomes `__` rather than `_` so `a.b` and `a_b` stay distinct.
pub fn wire_tool_name(name: &str) -> String {
    name.replace('.', "__")
}

/// The inverse map, built from the tools actually declared this session.
///
/// Inverting by string rewrite would be ambiguous; the executor is keyed by the
/// original namespaced name, and calling the wrong server is worse than failing.
#[derive(Debug, Default, Clone)]
pub struct ToolNames(std::collections::HashMap<String, String>);

impl ToolNames {
    pub fn new(tools: &[ToolDescriptor]) -> Self {
        Self(
            tools
                .iter()
                .map(|t| (wire_tool_name(&t.name), t.name.clone()))
                .collect(),
        )
    }

    /// Map a name the model used back to the declared one. An undeclared name
    /// passes through untouched so the executor reports "not found" rather than
    /// this layer guessing.
    pub fn restore(&self, wire_name: &str) -> String {
        self.0
            .get(wire_name)
            .cloned()
            .unwrap_or_else(|| wire_name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            requires_confirmation: false,
        }
    }

    #[test]
    fn the_namespace_dot_is_translated_for_the_wire() {
        assert_eq!(wire_tool_name("clock.now"), "clock__now");
        assert_eq!(wire_tool_name("plain"), "plain");
    }

    #[test]
    fn a_wire_name_maps_back_to_the_name_the_executor_is_keyed_by() {
        let names = ToolNames::new(&[tool("clock.now")]);
        assert_eq!(names.restore("clock__now"), "clock.now");
    }

    #[test]
    fn dotted_and_underscored_names_do_not_collide() {
        // `.` -> `_` would make these the same wire name, and the router would
        // then call whichever server won the map insert.
        let names = ToolNames::new(&[tool("a.b"), tool("a_b")]);
        assert_eq!(names.restore("a__b"), "a.b");
        assert_eq!(names.restore("a_b"), "a_b");
    }

    #[test]
    fn an_undeclared_name_passes_through_rather_than_being_guessed_at() {
        let names = ToolNames::new(&[tool("clock.now")]);
        assert_eq!(names.restore("ghost"), "ghost");
    }
}

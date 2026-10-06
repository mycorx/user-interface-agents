// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use serde_json::{Value, json};
use uia_core::tools::ToolDescriptor;

/// OpenAI takes JSON Schema directly as an object under `parameters`.
pub fn to_openai_declaration(t: &ToolDescriptor) -> Value {
    json!({
        "name": t.name,
        "description": t.description,
        "parameters": t.input_schema,
    })
}

/// Nova's toolSpec nests the schema as a JSON *string* under inputSchema.json.
pub fn to_nova_declaration(t: &ToolDescriptor) -> Value {
    json!({
        "toolSpec": {
            "name": t.name,
            "description": t.description,
            "inputSchema": { "json": t.input_schema.to_string() }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uia_core::tools::ToolDescriptor;

    fn sample() -> ToolDescriptor {
        ToolDescriptor {
            name: "clock.now".into(),
            description: "Current time".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "tz": { "type": "string", "description": "IANA zone" } },
                "required": ["tz"]
            }),
            requires_confirmation: false,
        }
    }

    #[test]
    fn openai_declaration_carries_name_description_and_parameters() {
        let d = to_openai_declaration(&sample());
        assert_eq!(d["name"], "clock.now");
        assert_eq!(d["description"], "Current time");
        assert_eq!(d["parameters"]["type"], "object");
        assert_eq!(d["parameters"]["properties"]["tz"]["type"], "string");
        assert_eq!(d["parameters"]["required"][0], "tz");
    }

    #[test]
    fn nova_declaration_serialises_the_schema_as_a_json_string() {
        // Nova's toolSpec carries the schema as a JSON *string*, not an object.
        // Getting this wrong yields a tool the model can see but cannot call.
        let d = to_nova_declaration(&sample());
        assert_eq!(d["toolSpec"]["name"], "clock.now");
        let schema_str = d["toolSpec"]["inputSchema"]["json"]
            .as_str()
            .expect("inputSchema.json must be a string");
        let reparsed: serde_json::Value = serde_json::from_str(schema_str).unwrap();
        assert_eq!(reparsed["properties"]["tz"]["type"], "string");
    }

    #[test]
    fn a_schema_with_no_properties_still_produces_a_valid_declaration() {
        let d = ToolDescriptor {
            name: "ping".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            requires_confirmation: false,
        };
        let g = to_openai_declaration(&d);
        assert_eq!(g["parameters"]["type"], "object");
        assert!(to_nova_declaration(&d)["toolSpec"]["inputSchema"]["json"].is_string());
    }
}

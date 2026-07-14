//! Flat input schema for the read-only semantic intelligence boundary.
use serde_json::{json, Value};

pub fn code_intelligence() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation": {"type": "string", "enum": ["definition", "references", "diagnostics", "rename_preview"]},
            "path": {"type": "string"},
            "line": {"type": "integer", "minimum": 1, "description": "One-based line number."},
            "column": {"type": "integer", "minimum": 0, "description": "Zero-based UTF-16 code-unit offset within the line; CodeWeave converts it to the server's negotiated position encoding."},
            "new_name": {"type": "string"},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20, "description": "Maximum references or diagnostics returned. Diagnostics also report total_count, result_count, and truncated."}
        },
        "required": ["operation"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

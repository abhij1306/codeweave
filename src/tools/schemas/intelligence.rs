//! Flat input schema for the read-only semantic intelligence boundary.
use serde_json::{json, Value};

pub fn code_intelligence() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation": {"type": "string", "enum": ["definition", "references", "diagnostics", "rename_preview"]},
            "path": {"type": "string"},
            "line": {"type": "integer", "minimum": 1, "description": "One-based UTF-16 line."},
            "column": {"type": "integer", "minimum": 0, "description": "Zero-based UTF-16 column."},
            "new_name": {"type": "string"},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64, "default": 20}
        },
        "required": ["operation"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

//! Input schemas for the edit tools: the narrow single-operation writers plus
//! the code_preview / code_transaction multi-file engine entry points.

use serde_json::{json, Value};

const HANDLE_DESCRIPTION: &str = "Range handle returned by code_retrieve. A handle-based change must be the only change for its file in one transaction; combining it with another change for that file is rejected as ambiguous.";
const OPTIONAL_HANDLE_DESCRIPTION: &str = "Optional range handle returned by code_retrieve. It scopes the exact-text match to the fetched range. A handle-based change must be the only change for its file in one transaction; combining it with another change for that file is rejected as ambiguous.";
const REPLACEMENT_TEXT_DESCRIPTION: &str = "Replacement text. When replacing text that ends with a terminal newline, omitting that newline here preserves it from the selected text or range.";

fn rollback_on_failure_schema() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "Deprecated compatibility field; its value is ignored. Validation failures are reported and edits are never rolled back. Long-running validation may continue in the background."
    })
}

pub fn code_write() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "content": {"type": "string"},
            "overwrite": {"type": "boolean", "default": true},
            "expected_hash": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
        },
        "required": ["path", "content"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_replace() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "old_text": {"type": "string"},
            "new_text": {"type": "string", "description": REPLACEMENT_TEXT_DESCRIPTION},
            "expected_replacements": {"type": "integer", "minimum": 1, "default": 1},
            "expected_hash": {"type": "string"},
            "handle": {"type": "string", "description": OPTIONAL_HANDLE_DESCRIPTION},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
        },
        "required": ["path", "old_text", "new_text"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_replace_range() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "handle": {"type": "string", "description": HANDLE_DESCRIPTION},
            "new_text": {"type": "string", "description": REPLACEMENT_TEXT_DESCRIPTION},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
        },
        "required": ["path", "handle", "new_text"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_insert() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "content": {"type": "string"},
            "anchor_symbol": {"type": "string"},
            "position": {"type": "string", "enum": ["before", "after", "inside_start", "inside_end"]},
            "expected_hash": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
        },
        "required": ["path", "content", "anchor_symbol", "position"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_delete() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "expected_hash": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
        },
        "required": ["path"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_rename() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "to": {"type": "string"},
            "expected_hash": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
        },
        "required": ["path", "to"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_preview() -> Value {
    json!({
        "type": "object",
        "properties": {
            "changes": {"type": "array", "items": change_schema()},
            "snapshot_id": {"type": "string"}
        },
        "required": ["changes"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_transaction() -> Value {
    json!({
        "type": "object",
        "properties": {
            "changes": {"type": "array", "items": change_schema()},
            "snapshot_id": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": rollback_on_failure_schema(),
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard", "description": "compact omits the unified diff and returns diff_stat only; standard caps the diff to bound payload size; debug returns the full diff."}
        },
        "required": ["changes"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

/// A deliberately flat superset schema. Per-kind required fields are published
/// by `code_capabilities`; conditional JSON Schema would make hosted clients
/// less reliable and is rejected by the registry's flat-schema checks.
fn change_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "enum": ["create", "replace", "replace_range", "insert", "delete", "rename"], "description": "Required operation kind."},
            "path": {"type": "string"},
            "to": {"type": "string"},
            "content": {"type": "string"},
            "old_text": {"type": "string"},
            "new_text": {"type": "string", "description": REPLACEMENT_TEXT_DESCRIPTION},
            "handle": {"type": "string", "description": format!("Optional for replace and required for replace_range. {HANDLE_DESCRIPTION}")},
            "anchor_symbol": {"type": "string"},
            "position": {"type": "string", "enum": ["before", "after", "inside_start", "inside_end"]},
            "overwrite": {"type": "boolean"},
            "expected_hash": {"type": "string"},
            "expected_replacements": {"type": "integer", "minimum": 1}
        },
        "required": ["kind"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_schemas_share_the_non_destructive_validation_contract() {
        let expected = rollback_on_failure_schema();
        assert_eq!(expected["default"], false);
        let description = expected["description"].as_str().unwrap();
        assert!(description.contains("Deprecated compatibility field"));
        assert!(description.contains("never rolled back"));
        for schema in [
            code_write(),
            code_replace(),
            code_replace_range(),
            code_insert(),
            code_delete(),
            code_rename(),
            code_transaction(),
        ] {
            assert_eq!(schema["properties"]["rollback_on_failure"], expected);
        }
    }

    #[test]
    fn replacement_schemas_document_handle_and_newline_contracts() {
        let replace = code_replace();
        let replace_range = code_replace_range();
        let change = change_schema();
        assert_eq!(
            change["properties"]["handle"]["description"],
            format!("Optional for replace and required for replace_range. {HANDLE_DESCRIPTION}")
        );

        for description in [
            replace["properties"]["handle"]["description"].as_str(),
            replace_range["properties"]["handle"]["description"].as_str(),
            change["properties"]["handle"]["description"].as_str(),
        ] {
            let description = description.expect("handle description");
            assert!(description.contains("code_retrieve"));
            assert!(description.contains("only change for its file"));
            assert!(description.contains("ambiguous"));
        }

        for description in [
            replace["properties"]["new_text"]["description"].as_str(),
            replace_range["properties"]["new_text"]["description"].as_str(),
            change["properties"]["new_text"]["description"].as_str(),
        ] {
            let description = description.expect("new_text description");
            assert!(description.contains("terminal newline"));
            assert!(description.contains("preserves"));
        }
    }
}

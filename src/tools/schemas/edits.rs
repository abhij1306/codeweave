//! Input schemas for the edit tools: the narrow single-operation writers plus
//! the code_preview / code_transaction multi-file engine entry points.

use serde_json::{json, Value};

pub fn code_write() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "content": {"type": "string"},
            "overwrite": {"type": "boolean", "default": true},
            "expected_hash": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": {"type": "boolean"},
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
            "new_text": {"type": "string"},
            "expected_replacements": {"type": "integer", "minimum": 1, "default": 1},
            "expected_hash": {"type": "string"},
            "handle": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": {"type": "boolean"},
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
            "handle": {"type": "string"},
            "new_text": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": {"type": "boolean"},
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
            "rollback_on_failure": {"type": "boolean"},
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
            "rollback_on_failure": {"type": "boolean"},
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
            "rollback_on_failure": {"type": "boolean"},
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
            "changes": {"type": "array", "items": {"type": "object"}},
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
            "changes": {"type": "array", "items": {"type": "object"}},
            "snapshot_id": {"type": "string"},
            "validate": {"type": "array", "items": {"type": "string"}},
            "rollback_on_failure": {"type": "boolean"},
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard", "description": "compact omits the unified diff and returns diff_stat only; standard caps the diff to bound payload size; debug returns the full diff."}
        },
        "required": ["changes"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

//! Input schemas for the git_* tools. Every git schema is closed
//! (`additionalProperties: false`) so spoofed fields are rejected at the schema
//! boundary.

use serde_json::{json, Value};

pub fn git_status() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_diff() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": {"type": "array", "items": {"type": "string"}},
            "staged": {"type": "boolean", "default": false},
            "max_chars": {"type": "integer", "minimum": 1, "maximum": 200000},
            "start_line": {"type": "integer", "minimum": 1},
            "end_line": {"type": "integer", "minimum": 1},
            "symbol": {"type": "string"},
            "hunk_ids": {"type": "array", "items": {"type": "string"}},
            "continuation": {"type": "string", "description": "Snapshot- and scope-bound continuation returned by a truncated git_diff response. Reuses the original paths, staged mode, symbol/line focus, and hunk IDs."}
        },
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_log() -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20}
        },
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_show() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ref": {"type": "string", "default": "HEAD"},
            "max_chars": {"type": "integer", "minimum": 1, "maximum": 200000}
        },
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_blame() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": {"type": "array", "items": {"type": "string"}},
            "start_line": {"type": "integer", "minimum": 1},
            "end_line": {"type": "integer", "minimum": 1},
            "max_chars": {"type": "integer", "minimum": 1, "maximum": 200000}
        },
        "required": ["paths"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_preflight() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_stage() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["paths"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_commit() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {"type": "string"}
        },
        "required": ["message"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_restore() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": {"type": "array", "items": {"type": "string"}},
            "staged": {"type": "boolean", "default": false},
            "confirm": {"type": "boolean"}
        },
        "required": ["paths", "confirm"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn git_push() -> Value {
    json!({
        "type": "object",
        "properties": {
            "remote": {"type": "string", "description": "Push remote name (default: origin)"},
            "branch": {"type": "string", "description": "Branch to push (default: current branch)"},
            "confirm": {"type": "boolean"}
        },
        "required": ["confirm"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

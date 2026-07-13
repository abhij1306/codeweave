//! Input schema for the `workspace` tool. Kept deliberately flat (no
//! oneOf/allOf/not/const) so hosted MCP clients send it cheaply and parse it
//! reliably every turn.

use serde_json::{json, Value};

pub fn workspace() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"default": "summary", "type": "string", "enum": ["summary", "refresh", "changes", "diagnostics", "skills", "skill"]},
            "skill_name": {"type": "string", "pattern": "^[A-Za-z0-9_-]+$", "description": "Skill directory name. Use only after an explicit user request to use that skill."},
            "force": {"type": "boolean"}
        },
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

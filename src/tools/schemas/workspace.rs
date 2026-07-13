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
            "force": {"type": "boolean"},
            "since_generation": {"type": "integer", "minimum": 0, "description": "For action=changes, return only mutations with a generation greater than this value."},
            "source": {"type": "string", "description": "For action=changes, return only mutations from this source."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 200, "description": "Maximum number of mutations returned by action=changes."}
        },
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

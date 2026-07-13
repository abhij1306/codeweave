//! Input schemas for the bash* tools. Closed schemas
//! (`additionalProperties: false`) so a spoofed `action`/`run_id` is rejected at
//! the schema boundary as well as in the compatibility layer.

use serde_json::{json, Value};

pub fn bash() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "minLength": 1, "description": "Command string passed to the configured Bash executable with -c."},
            "cwd": {"type": "string", "description": "Existing workspace-relative directory. Defaults to the workspace root."},
            "background": {"type": "boolean", "default": false, "description": "Run detached and return immediately with a run_id to poll."},
            "timeout_ms": {"type": "integer", "minimum": 1, "description": "Per-run timeout in ms (<= maxTimeoutMs). The command is killed if it exceeds this; the foreground budget only detaches, it does not kill."}
        },
        "required": ["command"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn bash_status() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "minLength": 1}
        },
        "required": ["run_id"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn bash_output() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "minLength": 1},
            "stream": {"type": "string", "enum": ["combined", "stdout", "stderr"]},
            "continuation": {"type": "string"}
        },
        "required": ["run_id"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn bash_cancel() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "minLength": 1}
        },
        "required": ["run_id"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::bash_contract;
    use std::collections::BTreeSet;

    #[test]
    fn bash_schemas_match_runtime_contract_tables() {
        for (name, schema) in [
            ("bash", bash()),
            ("bash_status", bash_status()),
            ("bash_output", bash_output()),
            ("bash_cancel", bash_cancel()),
        ] {
            let contract = bash_contract(name).expect("Bash contract");
            assert_eq!(schema["required"], json!(contract.required));
            let schema_fields = schema["properties"]
                .as_object()
                .expect("properties")
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let contract_fields = contract.allowed_fields().collect::<BTreeSet<_>>();
            assert_eq!(schema_fields, contract_fields, "{name}");
        }
    }
}

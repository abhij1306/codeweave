//! Input schemas for the single public retrieval tool and capability discovery.

use serde_json::{json, Value};

pub fn code_retrieve() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operations": {
                "type": "array",
                "minItems": 1,
                "maxItems": 12,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "minLength": 1, "maxLength": 80},
                        "operation": {"type": "string", "enum": ["find_file", "find_symbol", "search_text", "find_references", "symbols_overview", "repo_map", "read"]},
                        "name": {"type": "string", "minLength": 1, "description": "Filename substring or glob for find_file."},
                        "symbol": {"type": "string", "minLength": 1, "description": "Symbol selector for find_symbol or find_references."},
                        "pattern": {"type": "string", "minLength": 1, "description": "Literal text or regular expression for search_text."},
                        "syntax": {"type": "string", "enum": ["literal", "regex"], "default": "literal"},
                        "target": {"type": "string", "enum": ["path", "handle", "symbol", "metadata", "bash_status", "bash_log", "continuation"], "description": "Exact target kind for read."},
                        "value": {"type": "string", "minLength": 1, "description": "Exact target value for read."},
                        "path": {"type": "string", "description": "Single path for symbols_overview or optional symbol owner path for read."},
                        "paths": {"type": "array", "items": {"type": "string"}, "description": "Strict workspace-relative search scope."},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64},
                        "context_lines": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Search-result context lines."},
                        "case_sensitive": {"type": "boolean"},
                        "reference_scope": {"type": "string", "enum": ["all", "production", "tests"], "default": "all"},
                        "reference_kinds": {"type": "array", "items": {"type": "string", "enum": ["declaration", "call", "import", "type", "read", "write", "other"]}},
                        "definition_path": {"type": "string"},
                        "definition_line": {"type": "integer", "minimum": 1},
                        "start_line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64},
                        "end_line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64},
                        "surrounding_lines": {"type": "integer", "minimum": 0, "maximum": 200, "description": "Additional lines around an exact symbol read."},
                        "include_imports": {"type": "boolean"},
                        "max_chars": {"type": "integer", "minimum": 1, "maximum": 200000},
                        "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"}
                    },
                    "required": ["operation"],
                    "additionalProperties": false
                }
            },
            "fail_fast": {"type": "boolean", "default": false}
        },
        "required": ["operations"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_capabilities() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

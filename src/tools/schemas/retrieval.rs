//! Input schemas for the read-only retrieval tools: code_context,
//! code_capabilities, code_fetch, code_search.

use serde_json::{json, Value};

pub fn code_context() -> Value {
    json!({
        "type": "object",
        "properties": {
            "terms": {"minItems": 1, "maxItems": 12, "type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 80}},
            "paths": {"type": "array", "items": {"type": "string"}},
            "required_terms": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 80}},
            "optional_terms": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 80}},
            "exclude_terms": {"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 80}},
            "document_types": {"type": "array", "items": {"type": "string", "enum": ["source", "test", "instruction", "artifact", "runtime_evidence", "log"]}},
            "min_score": {"type": "number", "minimum": 0},
            "include_bash_failures": {"type": "boolean", "default": false, "description": "Include up to three recent Bash failures relevant to this query."}
        },
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_capabilities() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_fetch() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "start_line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64},
            "end_line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64},
            "items": {"type": "array", "items": {"type": "object", "properties": {"kind": {"type": "string", "enum": ["path", "handle", "symbol", "metadata", "bash_status", "bash_log", "continuation"]}, "value": {"type": "string", "minLength": 1}, "start_line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64}, "end_line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_i64}, "context_lines": {"type": "integer", "minimum": 0, "maximum": 200}, "include_imports": {"type": "boolean"}}, "required": ["kind", "value"], "additionalProperties": false}},
            "response_detail": {"type": "string", "enum": ["compact", "standard", "debug"], "default": "standard"},
            "max_chars": {"type": "integer", "minimum": 1, "maximum": 200000}
        },
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

pub fn code_search() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {"default": "", "type": "string"},
            "mode": {"type": "string", "enum": ["literal", "regex", "filename", "symbol", "references", "outline", "repo_map"]},
            "paths": {"type": "array", "items": {"type": "string"}, "description": "Strict workspace-relative path scope. repo_map returns only directories under these paths."},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 200},
            "context_lines": {"type": "integer", "minimum": 0, "maximum": 20},
            "case_sensitive": {"type": "boolean"}
        },
        "$schema": "http://json-schema.org/draft-07/schema#"
    })
}

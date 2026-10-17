//! The second adapter: Tree-sitter and live-index answers used when the
//! semantic backend is absent or its result cannot be trusted.
use super::live_index::LiveIndex;
use super::session::{FallbackReason, Session};
use crate::model::AppResult;
use crate::symbols::{extract_symbols, parse_has_error};
use serde_json::{json, Value};
use std::path::Path;

pub(crate) struct Fallback<'a> {
    pub(crate) live: &'a LiveIndex,
    pub(crate) workspace_id: &'a str,
}

impl Fallback<'_> {
    pub(crate) fn definition(
        &self,
        session: &Session<'_>,
        line: usize,
        reason: Option<FallbackReason>,
    ) -> Value {
        let symbol = extract_symbols(Path::new(&session.relative), &session.content)
            .into_iter()
            .find(|symbol| symbol.start_line <= line && symbol.end_line >= line);
        let mut response = json!({
            "operation": "definition",
            "backend": "fallback",
            "evidence": "syntactic",
            "result": {
                "evidence": "syntactic",
                "result": symbol.map(|symbol| json!({"path": session.relative, "symbol": symbol}))
            }
        });
        insert_reason(&mut response, reason, false);
        response
    }

    pub(crate) fn diagnostics(
        &self,
        session: &Session<'_>,
        max_results: usize,
        reason: Option<FallbackReason>,
    ) -> Value {
        let syntax_error =
            parse_has_error(Path::new(&session.relative), &session.content) == Some(true);
        let diagnostics = if syntax_error {
            vec![json!({"severity": "error", "message": "Tree-sitter reported a syntax error"})]
        } else {
            Vec::new()
        };
        let limited = LimitedDiagnostics::new(diagnostics, max_results);
        let semantic_available = reason.is_none();
        let mut response = json!({
            "operation": "diagnostics",
            "backend": "fallback",
            "evidence": "syntactic",
            "status": if semantic_available { "available" } else { "partial" },
            "diagnostics_available": semantic_available,
            "semantic_diagnostics_available": false,
            "diagnostic_scope": "syntax_only",
            "total_count": limited.total_count,
            "result_count": limited.items.len(),
            "truncated": limited.truncated(),
            "result": {"evidence": "syntactic", "diagnostics": limited.items}
        });
        insert_reason(&mut response, reason, true);
        response
    }

    pub(crate) fn references(
        &self,
        session: &Session<'_>,
        line: usize,
        max_results: usize,
        reason: Option<FallbackReason>,
    ) -> AppResult<Value> {
        let snapshot_id = self.live.snapshot_id();
        let mut response = self.live.references(|references| {
            references.fallback_at_position(
                self.workspace_id,
                &snapshot_id,
                &session.relative,
                line,
                max_results,
            )
        })?;
        insert_reason(&mut response, reason, false);
        Ok(response)
    }
}

/// Diagnostics bounded to the requested count, remembering the total.
pub(crate) struct LimitedDiagnostics {
    pub(crate) items: Vec<Value>,
    pub(crate) total_count: usize,
}

impl LimitedDiagnostics {
    pub(crate) fn new(mut diagnostics: Vec<Value>, max_results: usize) -> Self {
        let total_count = diagnostics.len();
        diagnostics.truncate(max_results);
        Self {
            items: diagnostics,
            total_count,
        }
    }

    pub(crate) fn truncated(&self) -> bool {
        self.total_count > self.items.len()
    }
}

fn insert_reason(response: &mut Value, reason: Option<FallbackReason>, with_backend: bool) {
    let (Some(object), Some(reason)) = (response.as_object_mut(), reason) else {
        return;
    };
    object.insert("fallback_reason".to_owned(), json!(reason.error.0));
    if with_backend {
        object.insert("semantic_backend".to_owned(), reason.backend_status);
    }
}

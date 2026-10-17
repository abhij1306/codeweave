//! Compile an LSP `WorkspaceEdit` into typed whole-file replacements and
//! hand them to the edit engine.
use super::position::{lsp_position, offset_for_server_position};
use super::protocol::PositionEncoding;
use super::session::ResultFiles;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::index::{content_hash, ContentHash};
use crate::requests::edit::Change;
use serde_json::{json, Value};

/// One file's complete before/after text, guarded by the hash of `old_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenameChange {
    pub(crate) path: String,
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    pub(crate) expected_hash: ContentHash,
}

impl RenameChange {
    /// The public `replace` change shape shared by previews and transactions.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "kind": "replace",
            "path": self.path,
            "old_text": self.old_text,
            "new_text": self.new_text,
            "expected_replacements": 1,
            "expected_hash": self.expected_hash
        })
    }

    /// The same change as a typed edit-engine change.
    pub(crate) fn to_change(&self) -> Change {
        Change::Replace {
            path: self.path.clone(),
            old_text: self.old_text.clone(),
            new_text: self.new_text.clone(),
            expected_replacements: 1,
            expected_hash: Some(self.expected_hash.to_string()),
            handle: None,
        }
    }
}

fn invalid_edit(message: &str) -> AppError {
    AppError::new(ErrorCode::InvalidLspEdit, message.to_owned())
}

/// Compile `edit`. Every file is read through `files`, so each change's
/// `old_text` and `expected_hash` describe confined, index-current content.
pub(crate) fn workspace_edit_changes(
    files: &mut ResultFiles<'_>,
    edit: &Value,
    encoding: PositionEncoding,
) -> AppResult<Vec<RenameChange>> {
    let mut documents: Vec<(&str, &Value)> = Vec::new();
    if let Some(object) = edit.get("changes").and_then(Value::as_object) {
        documents.extend(object.iter().map(|(uri, edits)| (uri.as_str(), edits)));
    }
    if let Some(items) = edit.get("documentChanges").and_then(Value::as_array) {
        for item in items {
            if item.get("kind").is_some() {
                return Err(AppError::new(
                    ErrorCode::UnsupportedWorkspaceEdit,
                    "LSP resource create/rename/delete operations are not supported",
                ));
            }
            let uri = item["textDocument"]["uri"]
                .as_str()
                .ok_or_else(|| invalid_edit("TextDocumentEdit lacks a URI"))?;
            documents.push((uri, &item["edits"]));
        }
    }
    if documents.is_empty() {
        return Err(AppError::new(
            ErrorCode::UnsupportedWorkspaceEdit,
            "WorkspaceEdit contains no supported text edits",
        ));
    }

    let mut output = Vec::with_capacity(documents.len());
    for (uri, edits) in documents {
        let path = files.relative(uri)?;
        let before = files.content(&path)?.to_owned();
        let mut ranges = Vec::new();
        for item in edits
            .as_array()
            .ok_or_else(|| invalid_edit("Edits must be an array"))?
        {
            let (start_line, start_character) = lsp_position(&item["range"]["start"])?;
            let (end_line, end_character) = lsp_position(&item["range"]["end"])?;
            let start = offset_for_server_position(&before, start_line, start_character, encoding)?;
            let end = offset_for_server_position(&before, end_line, end_character, encoding)?;
            if end < start {
                return Err(invalid_edit("LSP edit range ends before it starts"));
            }
            let text = item["newText"]
                .as_str()
                .ok_or_else(|| invalid_edit("LSP edit lacks newText"))?;
            ranges.push((start, end, text));
        }
        ranges.sort_by_key(|range| range.0);
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(AppError::new(
                ErrorCode::OverlappingLspEdits,
                "LSP returned overlapping edits",
            ));
        }
        let mut after = before.clone();
        for (start, end, text) in ranges.into_iter().rev() {
            after.replace_range(start..end, text);
        }
        output.push(RenameChange {
            expected_hash: content_hash(&before),
            path,
            old_text: before,
            new_text: after,
        });
    }
    Ok(output)
}

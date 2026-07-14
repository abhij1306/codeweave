use super::normalize::{offset_for_server_position, uri_path, workspace_relative_path};
use super::protocol::PositionEncoding;
use crate::model::{AppError, AppResult};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub(crate) fn workspace_edit_changes(
    root: &Path,
    edit: &Value,
    encoding: PositionEncoding,
) -> AppResult<Vec<Value>> {
    let mut documents: Vec<(String, Value)> = Vec::new();
    if let Some(object) = edit.get("changes").and_then(Value::as_object) {
        documents.extend(
            object
                .iter()
                .map(|(uri, edits)| (uri.clone(), edits.clone())),
        );
    }
    if let Some(items) = edit.get("documentChanges").and_then(Value::as_array) {
        for item in items {
            if item.get("kind").is_some() {
                return Err(AppError::new(
                    "UNSUPPORTED_WORKSPACE_EDIT",
                    "LSP resource create/rename/delete operations are not supported",
                ));
            }
            let uri = item["textDocument"]["uri"]
                .as_str()
                .ok_or_else(|| AppError::new("INVALID_LSP_EDIT", "TextDocumentEdit lacks a URI"))?;
            documents.push((uri.to_owned(), item["edits"].clone()));
        }
    }
    if documents.is_empty() {
        return Err(AppError::new(
            "UNSUPPORTED_WORKSPACE_EDIT",
            "WorkspaceEdit contains no supported text edits",
        ));
    }

    let canonical_root = root.canonicalize()?;
    let mut output = Vec::new();
    for (uri, edits) in documents {
        let path = uri_path(&canonical_root, &uri)?;
        let before = fs::read_to_string(&path)?;
        let mut ranges = Vec::new();
        for item in edits
            .as_array()
            .ok_or_else(|| AppError::new("INVALID_LSP_EDIT", "Edits must be an array"))?
        {
            let range = &item["range"];
            let start = offset_for_server_position(
                &before,
                range["start"]["line"].as_u64().unwrap_or(0) as usize,
                range["start"]["character"].as_u64().unwrap_or(0) as usize,
                encoding,
            )?;
            let end = offset_for_server_position(
                &before,
                range["end"]["line"].as_u64().unwrap_or(0) as usize,
                range["end"]["character"].as_u64().unwrap_or(0) as usize,
                encoding,
            )?;
            ranges.push((
                start,
                end,
                item["newText"].as_str().unwrap_or("").to_owned(),
            ));
        }
        ranges.sort_by_key(|range| range.0);
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(AppError::new(
                "OVERLAPPING_LSP_EDITS",
                "LSP returned overlapping edits",
            ));
        }
        let mut after = before.clone();
        for (start, end, text) in ranges.into_iter().rev() {
            after.replace_range(start..end, &text);
        }
        let relative = workspace_relative_path(&canonical_root, &path)?;
        let expected_hash = crate::index::content_hash(&before);
        output.push(json!({
            "kind": "replace",
            "path": relative,
            "old_text": before,
            "new_text": after,
            "expected_replacements": 1,
            "expected_hash": expected_hash
        }));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::normalize::path_uri;

    #[test]
    fn workspace_edit_uses_negotiated_utf8_ranges() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sample.py");
        fs::write(&path, "def café():\n    return 1\n").unwrap();
        let edit = json!({"changes":{path_uri(&path):[{
            "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":9}},
            "newText":"bistro"
        }]}});
        let changes = workspace_edit_changes(root.path(), &edit, PositionEncoding::Utf8).unwrap();
        assert_eq!(changes[0]["kind"], "replace");
        assert!(changes[0]["new_text"].as_str().unwrap().contains("bistro"));
        assert_eq!(
            changes[0]["expected_hash"],
            crate::index::content_hash("def café():\n    return 1\n")
        );
    }
}

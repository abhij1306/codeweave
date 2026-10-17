//! Position-encoding conversion and normalization of LSP location responses
//! into the public contract (one-based lines, zero-based UTF-16 columns).
use super::protocol::PositionEncoding;
use super::request::Position;
use super::session::ResultFiles;
use crate::model::{AppError, AppResult};
use crate::reference_service::{ReferencePosition, ReferenceRange, SemanticReferenceLocation};
use serde_json::{json, Value};

fn invalid_position(message: &str) -> AppError {
    AppError::new("INVALID_LSP_POSITION", message.to_owned())
}

fn line_content(content: &str, line: usize) -> AppResult<&str> {
    content
        .split('\n')
        .nth(line)
        .ok_or_else(|| invalid_position("LSP position line is outside the document"))
}

fn prefix_for_utf16_column(line: &str, column: usize) -> AppResult<&str> {
    let mut units = 0usize;
    for (index, ch) in line.char_indices() {
        if units == column {
            return Ok(&line[..index]);
        }
        units += ch.len_utf16();
        if units > column {
            return Err(invalid_position(
                "UTF-16 column splits a Unicode scalar value",
            ));
        }
    }
    if units == column {
        Ok(line)
    } else {
        Err(invalid_position("UTF-16 column is outside the line"))
    }
}

fn prefix_for_server_column(
    line: &str,
    column: usize,
    encoding: PositionEncoding,
) -> AppResult<&str> {
    match encoding {
        PositionEncoding::Utf8 => {
            if column <= line.len() && line.is_char_boundary(column) {
                Ok(&line[..column])
            } else {
                Err(invalid_position(
                    "UTF-8 column is outside a character boundary",
                ))
            }
        }
        PositionEncoding::Utf16 => prefix_for_utf16_column(line, column),
        PositionEncoding::Utf32 => match line.char_indices().nth(column) {
            Some((index, _)) => Ok(&line[..index]),
            None if line.chars().count() == column => Ok(line),
            None => Err(invalid_position("UTF-32 column is outside the line")),
        },
    }
}

pub(crate) fn server_character_from_utf16(
    content: &str,
    one_based_line: usize,
    utf16_column: usize,
    encoding: PositionEncoding,
) -> AppResult<usize> {
    let line = line_content(content, one_based_line.saturating_sub(1))?;
    let prefix = prefix_for_utf16_column(line, utf16_column)?;
    Ok(match encoding {
        PositionEncoding::Utf8 => prefix.len(),
        PositionEncoding::Utf16 => utf16_column,
        PositionEncoding::Utf32 => prefix.chars().count(),
    })
}

pub(crate) fn utf16_character_from_server(
    content: &str,
    zero_based_line: usize,
    server_column: usize,
    encoding: PositionEncoding,
) -> AppResult<usize> {
    let line = line_content(content, zero_based_line)?;
    let prefix = prefix_for_server_column(line, server_column, encoding)?;
    Ok(prefix.encode_utf16().count())
}

/// Byte offset of an LSP position in `content`.
pub(crate) fn offset_for_server_position(
    content: &str,
    zero_based_line: usize,
    server_column: usize,
    encoding: PositionEncoding,
) -> AppResult<usize> {
    let mut base = 0usize;
    let mut selected = None;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        if index == zero_based_line {
            selected = Some(line.strip_suffix('\n').unwrap_or(line));
            break;
        }
        base += line.len();
    }
    if selected.is_none() && zero_based_line == content.lines().count() && content.ends_with('\n') {
        selected = Some("");
        base = content.len();
    }
    let line = selected
        .ok_or_else(|| AppError::new("INVALID_LSP_EDIT", "Edit line is outside the file"))?;
    let prefix = prefix_for_server_column(line, server_column, encoding)?;
    Ok(base + prefix.len())
}

/// `TextDocumentPositionParams` for a public position in `content`.
pub(crate) fn position_params(
    uri: &str,
    content: &str,
    position: Position,
    encoding: PositionEncoding,
) -> AppResult<Value> {
    let character = server_character_from_utf16(content, position.line, position.column, encoding)?;
    Ok(json!({
        "textDocument": {"uri": uri},
        "position": {"line": position.line.saturating_sub(1), "character": character}
    }))
}

/// A zero-based LSP `Position` object as `(line, character)`.
pub(crate) fn lsp_position(value: &Value) -> AppResult<(usize, usize)> {
    let number = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(|| {
                AppError::details(
                    "INVALID_LSP_RESPONSE",
                    format!("LSP position lacks a numeric {key}"),
                    json!({"position": value}),
                )
            })
    };
    Ok((number("line")?, number("character")?))
}

fn location_items(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        item => vec![item],
    }
}

fn normalize_location(
    files: &mut ResultFiles<'_>,
    item: &Value,
    encoding: PositionEncoding,
) -> AppResult<SemanticReferenceLocation> {
    let uri = item
        .get("uri")
        .or_else(|| item.get("targetUri"))
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("INVALID_LSP_RESPONSE", "Location lacks URI"))?;
    let range = item
        .get("range")
        .or_else(|| item.get("targetSelectionRange"))
        .ok_or_else(|| AppError::new("INVALID_LSP_RESPONSE", "Location lacks range"))?;
    let relative = files.relative(uri)?;
    let content = files.content(&relative)?;
    let (start_line, start_character) = lsp_position(&range["start"])?;
    let (end_line, end_character) = lsp_position(&range["end"])?;
    let start = utf16_character_from_server(content, start_line, start_character, encoding)?;
    let end = utf16_character_from_server(content, end_line, end_character, encoding)?;
    Ok(SemanticReferenceLocation {
        path: relative,
        range: ReferenceRange {
            start: ReferencePosition {
                line: start_line + 1,
                column: start + 1,
                byte: None,
            },
            end: ReferencePosition {
                line: end_line + 1,
                column: end + 1,
                byte: None,
            },
        },
    })
}

/// Normalize a `Location | Location[] | LocationLink[] | null` response.
pub(crate) fn normalize_reference_locations(
    files: &mut ResultFiles<'_>,
    value: &Value,
    encoding: PositionEncoding,
) -> AppResult<Vec<SemanticReferenceLocation>> {
    location_items(value)
        .into_iter()
        .map(|item| normalize_location(files, item, encoding))
        .collect()
}

/// Definition results in the public `{path, line, column, end_line, end_column}` shape.
pub(crate) fn normalize_locations(
    files: &mut ResultFiles<'_>,
    value: &Value,
    encoding: PositionEncoding,
) -> AppResult<Vec<Value>> {
    Ok(normalize_reference_locations(files, value, encoding)?
        .into_iter()
        .map(|location| {
            json!({
                "path": location.path,
                "line": location.range.start.line,
                "column": location.range.start.column.saturating_sub(1),
                "end_line": location.range.end.line,
                "end_column": location.range.end.column.saturating_sub(1),
                "evidence": "semantic"
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_columns_convert_between_utf16_utf8_and_utf32() {
        let content = "a😀éz\n";
        let converted = [
            server_character_from_utf16(content, 1, 3, PositionEncoding::Utf8).unwrap(),
            server_character_from_utf16(content, 1, 3, PositionEncoding::Utf32).unwrap(),
            utf16_character_from_server(content, 0, 5, PositionEncoding::Utf8).unwrap(),
            utf16_character_from_server(content, 0, 2, PositionEncoding::Utf32).unwrap(),
            utf16_character_from_server(content, 0, 4, PositionEncoding::Utf32).unwrap(),
        ];
        assert_eq!(converted, [5, 2, 3, 3, 5]);
        assert_eq!(
            server_character_from_utf16(content, 1, 2, PositionEncoding::Utf8)
                .unwrap_err()
                .0
                .code,
            "INVALID_LSP_POSITION"
        );
    }

    #[test]
    fn edit_offsets_cover_line_ends_and_the_final_empty_line() {
        let content = "ab\ncd\n";
        let offsets = [(0, 2), (1, 1), (2, 0)].map(|(line, column)| {
            offset_for_server_position(content, line, column, PositionEncoding::Utf16).unwrap()
        });
        assert_eq!(offsets, [2, 4, 6]);
        assert_eq!(
            offset_for_server_position(content, 3, 0, PositionEncoding::Utf16)
                .unwrap_err()
                .0
                .code,
            "INVALID_LSP_EDIT"
        );
    }

    #[test]
    fn lsp_positions_without_numbers_are_protocol_errors() {
        assert_eq!(
            lsp_position(&json!({"line": 2, "character": 7})).unwrap(),
            (2, 7)
        );
        assert_eq!(
            lsp_position(&json!({"line": "2", "character": 7}))
                .unwrap_err()
                .0
                .code,
            "INVALID_LSP_RESPONSE"
        );
    }
}

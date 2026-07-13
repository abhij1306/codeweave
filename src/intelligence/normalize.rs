use super::protocol::PositionEncoding;
use crate::model::{AppError, AppResult};
use codeweave_rust::reference_service::{
    ReferencePosition, ReferenceRange, SemanticReferenceLocation,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn path_uri(path: &Path) -> String {
    let mut raw = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = raw.strip_prefix("//?/UNC/") {
        raw = format!("//{rest}");
    } else if let Some(rest) = raw.strip_prefix("//?/") {
        raw = rest.to_owned();
    }

    if let Some(unc) = raw.strip_prefix("//") {
        let (authority, path) = unc.split_once('/').unwrap_or((unc, ""));
        return format!(
            "file://{}/{}",
            percent_encode_uri_component(authority, false),
            percent_encode_uri_component(path, true)
        );
    }

    format!(
        "file:///{}",
        percent_encode_uri_component(raw.trim_start_matches('/'), true)
    )
}

fn percent_encode_uri_component(value: &str, preserve_path_separators: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (preserve_path_separators && matches!(byte, b'/' | b':')) {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn percent_decode_uri(value: &str) -> AppResult<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(AppError::new(
                    "INVALID_LSP_URI",
                    "LSP file URI contains an incomplete percent escape",
                ));
            }
            let pair = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| {
                AppError::new("INVALID_LSP_URI", "LSP file URI contains invalid UTF-8")
            })?;
            let byte = u8::from_str_radix(pair, 16).map_err(|_| {
                AppError::new(
                    "INVALID_LSP_URI",
                    "LSP file URI contains an invalid percent escape",
                )
            })?;
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| AppError::new("INVALID_LSP_URI", "LSP file URI is not valid UTF-8"))
}

fn path_within_root(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        let root = root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|remaining| remaining.starts_with('\\'))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

pub(crate) fn uri_path(root: &Path, uri: &str) -> AppResult<PathBuf> {
    let raw = uri
        .strip_prefix("file://")
        .ok_or_else(|| AppError::new("UNSUPPORTED_LSP_URI", "LSP returned a non-file URI"))?;
    let decoded = percent_decode_uri(raw)?;

    #[cfg(windows)]
    let path = {
        let normalized = decoded.replace('/', "\\");
        if !raw.starts_with('/') {
            PathBuf::from(format!("\\\\{normalized}"))
        } else if normalized.len() >= 3
            && normalized.starts_with('\\')
            && normalized.as_bytes()[2] == b':'
        {
            PathBuf::from(&normalized[1..])
        } else {
            PathBuf::from(normalized)
        }
    };
    #[cfg(not(windows))]
    let path = PathBuf::from(decoded);

    let canonical = path.canonicalize()?;
    let canonical_root = root.canonicalize()?;
    if !path_within_root(&canonical, &canonical_root) {
        return Err(AppError::new(
            "OUTSIDE_ROOT",
            "LSP result is outside the workspace",
        ));
    }
    Ok(canonical)
}

pub(crate) fn workspace_relative_path(root: &Path, path: &Path) -> AppResult<String> {
    let canonical_root = root.canonicalize()?;
    let canonical_path = path.canonicalize()?;
    if !path_within_root(&canonical_path, &canonical_root) {
        return Err(AppError::new(
            "OUTSIDE_ROOT",
            "LSP result is outside the workspace",
        ));
    }

    #[cfg(windows)]
    {
        let root = canonical_root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_owned();
        let path = canonical_path
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_owned();
        let relative = if path.eq_ignore_ascii_case(&root) {
            ""
        } else {
            path.get(root.len()..).ok_or_else(|| {
                AppError::new(
                    "OUTSIDE_ROOT",
                    "LSP result could not be made workspace-relative",
                )
            })?
        };
        Ok(relative.trim_start_matches('\\').replace('\\', "/"))
    }

    #[cfg(not(windows))]
    {
        let relative = canonical_path.strip_prefix(&canonical_root).map_err(|_| {
            AppError::new(
                "OUTSIDE_ROOT",
                "LSP result could not be made workspace-relative",
            )
        })?;
        Ok(relative.to_string_lossy().replace('\\', "/"))
    }
}

fn line_content(content: &str, line: usize) -> AppResult<&str> {
    content.split('\n').nth(line).ok_or_else(|| {
        AppError::new(
            "INVALID_LSP_POSITION",
            "LSP position line is outside the document",
        )
    })
}

fn prefix_for_utf16_column(line: &str, column: usize) -> AppResult<&str> {
    let mut units = 0usize;
    for (index, ch) in line.char_indices() {
        if units == column {
            return Ok(&line[..index]);
        }
        units += ch.len_utf16();
        if units > column {
            return Err(AppError::new(
                "INVALID_LSP_POSITION",
                "UTF-16 column splits a Unicode scalar value",
            ));
        }
    }
    if units == column {
        Ok(line)
    } else {
        Err(AppError::new(
            "INVALID_LSP_POSITION",
            "UTF-16 column is outside the line",
        ))
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
                Err(AppError::new(
                    "INVALID_LSP_POSITION",
                    "UTF-8 column is outside a character boundary",
                ))
            }
        }
        PositionEncoding::Utf16 => prefix_for_utf16_column(line, column),
        PositionEncoding::Utf32 => {
            if column == 0 {
                return Ok("");
            }
            let mut scalars = 0usize;
            for (index, _) in line.char_indices() {
                if scalars == column {
                    return Ok(&line[..index]);
                }
                scalars += 1;
            }
            if scalars == column {
                Ok(line)
            } else {
                Err(AppError::new(
                    "INVALID_LSP_POSITION",
                    "UTF-32 column is outside the line",
                ))
            }
        }
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

pub(crate) fn position_params(
    path: &Path,
    content: &str,
    line: usize,
    utf16_column: usize,
    encoding: PositionEncoding,
) -> AppResult<Value> {
    let character = server_character_from_utf16(content, line, utf16_column, encoding)?;
    Ok(json!({
        "textDocument": {"uri": path_uri(path)},
        "position": {"line": line.saturating_sub(1), "character": character}
    }))
}

fn location_items(value: &Value) -> Vec<Value> {
    if let Some(array) = value.as_array() {
        array.clone()
    } else if value.is_null() {
        Vec::new()
    } else {
        vec![value.clone()]
    }
}

fn normalize_location_parts(
    canonical_root: &Path,
    item: &Value,
    encoding: PositionEncoding,
) -> AppResult<(String, ReferenceRange)> {
    let uri = item
        .get("uri")
        .or_else(|| item.get("targetUri"))
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("INVALID_LSP_RESPONSE", "Location lacks URI"))?;
    let range = item
        .get("range")
        .or_else(|| item.get("targetSelectionRange"))
        .ok_or_else(|| AppError::new("INVALID_LSP_RESPONSE", "Location lacks range"))?;
    let path = uri_path(canonical_root, uri)?;
    let relative = workspace_relative_path(canonical_root, &path)?;
    let content = fs::read_to_string(&path)?;
    let start_line = range["start"]["line"].as_u64().unwrap_or(0) as usize;
    let end_line = range["end"]["line"].as_u64().unwrap_or(0) as usize;
    let start = utf16_character_from_server(
        &content,
        start_line,
        range["start"]["character"].as_u64().unwrap_or(0) as usize,
        encoding,
    )?;
    let end = utf16_character_from_server(
        &content,
        end_line,
        range["end"]["character"].as_u64().unwrap_or(0) as usize,
        encoding,
    )?;
    Ok((
        relative,
        ReferenceRange {
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
    ))
}

pub(crate) fn normalize_locations(
    root: &Path,
    value: &Value,
    encoding: PositionEncoding,
) -> AppResult<Vec<Value>> {
    let canonical_root = root.canonicalize()?;
    location_items(value)
        .into_iter()
        .map(|item| {
            let (relative, range) = normalize_location_parts(&canonical_root, &item, encoding)?;
            Ok(json!({
                "path": relative,
                "line": range.start.line,
                "column": range.start.column.saturating_sub(1),
                "end_line": range.end.line,
                "end_column": range.end.column.saturating_sub(1),
                "evidence": "semantic"
            }))
        })
        .collect()
}

pub(crate) fn normalize_reference_locations(
    root: &Path,
    value: &Value,
    encoding: PositionEncoding,
) -> AppResult<Vec<SemanticReferenceLocation>> {
    let canonical_root = root.canonicalize()?;
    location_items(value)
        .into_iter()
        .map(|item| {
            let (path, range) = normalize_location_parts(&canonical_root, &item, encoding)?;
            Ok(SemanticReferenceLocation { path, range })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_columns_convert_between_utf16_utf8_and_utf32() {
        let content = "a😀éz\n";
        assert_eq!(
            server_character_from_utf16(content, 1, 3, PositionEncoding::Utf8).unwrap(),
            5
        );
        assert_eq!(
            server_character_from_utf16(content, 1, 3, PositionEncoding::Utf32).unwrap(),
            2
        );
        assert_eq!(
            utf16_character_from_server(content, 0, 5, PositionEncoding::Utf8).unwrap(),
            3
        );
        assert_eq!(
            utf16_character_from_server(content, 0, 2, PositionEncoding::Utf32).unwrap(),
            3
        );
        assert!(server_character_from_utf16(content, 1, 2, PositionEncoding::Utf8).is_err());
    }

    #[test]
    fn file_uri_round_trips_spaces_and_unicode() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("café sample.py");
        fs::write(&path, "value = 1\n").unwrap();
        let uri = path_uri(&path);
        assert!(uri.contains("%20"));
        assert!(uri.contains("%C3%A9"));
        assert_eq!(
            uri_path(root.path(), &uri).unwrap(),
            path.canonicalize().unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_strips_windows_verbatim_prefix() {
        let uri = path_uri(Path::new(r"\\?\C:\Projects\Code Weave\sample.py"));
        assert_eq!(uri, "file:///C:/Projects/Code%20Weave/sample.py");
    }
}

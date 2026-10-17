//! `file://` URI codec: the only place that converts between LSP document URIs
//! and workspace-relative paths.
//!
//! Decoding is purely lexical. A URI is accepted only when its decoded path
//! sits under the workspace root (as configured or canonicalized) and its
//! remainder contains no `.`/`..` segments. Symlinks are deliberately not
//! resolved here: every read of a decoded path goes through the confined
//! workspace reader, which refuses symlinks and escapes at open time.
use crate::error::{AppError, AppResult, ErrorCode};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct UriCodec {
    root: PathBuf,
    /// Comparable forms of the root: as configured and canonicalized.
    prefixes: Vec<String>,
}

impl UriCodec {
    pub(crate) fn new(root: &Path) -> Self {
        let mut prefixes = vec![comparable(root)];
        if let Ok(canonical) = root.canonicalize() {
            let canonical = comparable(&canonical);
            if !prefixes.contains(&canonical) {
                prefixes.push(canonical);
            }
        }
        Self {
            root: root.to_owned(),
            prefixes,
        }
    }

    pub(crate) fn root_uri(&self) -> String {
        path_uri(&self.root)
    }

    pub(crate) fn document_uri(&self, relative: &str) -> String {
        path_uri(&self.root.join(relative))
    }

    /// Decode `uri` to a validated workspace-relative path using `/` separators.
    pub(crate) fn relative(&self, uri: &str) -> AppResult<String> {
        let decoded = comparable_text(&decode_file_uri(uri)?);
        let remainder = self
            .prefixes
            .iter()
            .find_map(|prefix| strip_root(&decoded, prefix))
            .ok_or_else(outside_root)?;
        if remainder.is_empty() {
            return Err(AppError::new(
                ErrorCode::InvalidLspUri,
                "LSP URI names the workspace root, not a file",
            ));
        }
        if remainder
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
        {
            return Err(outside_root());
        }
        Ok(remainder.to_owned())
    }
}

fn outside_root() -> AppError {
    AppError::new(
        ErrorCode::OutsideRoot,
        "LSP result is outside the workspace",
    )
}

/// Encode an absolute filesystem path as a `file://` URI.
pub(crate) fn path_uri(path: &Path) -> String {
    let raw = comparable_text(&path.to_string_lossy());
    if let Some(unc) = raw.strip_prefix("//") {
        let (authority, path) = unc.split_once('/').unwrap_or((unc, ""));
        return format!(
            "file://{}/{}",
            percent_encode(authority, false),
            percent_encode(path, true)
        );
    }
    format!(
        "file:///{}",
        percent_encode(raw.trim_start_matches('/'), true)
    )
}

/// Platform path text with `/` separators, the Windows verbatim prefix
/// removed, and no trailing separator.
fn comparable_text(raw: &str) -> String {
    let mut text = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.to_owned()
    };
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        text = format!("//{rest}");
    } else if let Some(rest) = text.strip_prefix("//?/") {
        text = rest.to_owned();
    }
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    text
}

fn comparable(path: &Path) -> String {
    comparable_text(&path.to_string_lossy())
}

fn strip_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let head = path.get(..root.len())?;
    let same = if cfg!(windows) {
        head.eq_ignore_ascii_case(root)
    } else {
        head == root
    };
    if !same {
        return None;
    }
    let rest = &path[root.len()..];
    if rest.is_empty() {
        Some(rest)
    } else {
        rest.strip_prefix('/')
    }
}

/// Decode a `file://` URI into platform path text.
fn decode_file_uri(uri: &str) -> AppResult<String> {
    let raw = uri.strip_prefix("file://").ok_or_else(|| {
        AppError::new(ErrorCode::UnsupportedLspUri, "LSP returned a non-file URI")
    })?;
    let (authority, path) = match raw.find('/') {
        Some(index) => (&raw[..index], &raw[index..]),
        None => (raw, ""),
    };
    let path = percent_decode(path)?;
    let authority = percent_decode(authority)?;
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        if cfg!(windows) {
            return Ok(format!("//{authority}{path}"));
        }
        return Err(AppError::new(
            ErrorCode::UnsupportedLspUri,
            "LSP returned a file URI with a remote authority",
        ));
    }
    let bytes = path.as_bytes();
    if cfg!(windows) && bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
        return Ok(path[1..].to_owned());
    }
    Ok(path)
}

fn percent_encode(value: &str, preserve_path_separators: bool) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (preserve_path_separators && matches!(byte, b'/' | b':')) {
            encoded.push(byte as char);
        } else {
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn percent_decode(value: &str) -> AppResult<String> {
    let invalid = |message: &str| AppError::new(ErrorCode::InvalidLspUri, message.to_owned());
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .ok_or_else(|| invalid("LSP file URI contains an incomplete percent escape"))?;
            let byte = u8::from_str_radix(pair, 16)
                .map_err(|_| invalid("LSP file URI contains an invalid percent escape"))?;
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid("LSP file URI is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_uris_round_trip_spaces_and_unicode() {
        let root = tempfile::tempdir().unwrap();
        let codec = UriCodec::new(root.path());
        let uri = codec.document_uri("src/café sample.py");
        assert!(uri.ends_with("/src/caf%C3%A9%20sample.py"), "{uri}");
        assert_eq!(codec.relative(&uri).unwrap(), "src/café sample.py");
    }

    #[test]
    fn equivalent_escapes_decode_to_the_same_relative_path() {
        let root = tempfile::tempdir().unwrap();
        let codec = UriCodec::new(root.path());
        let upper = codec.document_uri("café.py");
        let lower = upper.replace("%C3%A9", "%c3%a9");
        assert_ne!(upper, lower);
        assert_eq!(codec.relative(&lower).unwrap(), "café.py");
    }

    #[test]
    fn uris_outside_the_root_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let codec = UriCodec::new(root.path());
        let escaping = format!("{}/../secret.rs", codec.root_uri());
        let elsewhere = path_uri(&outside.path().join("secret.rs"));
        let sibling = format!("{}-sibling/a.rs", codec.root_uri());

        let codes = [
            escaping,
            elsewhere,
            sibling,
            "https://example.com/a.rs".into(),
        ]
        .map(|uri| codec.relative(&uri).unwrap_err().code().as_str());

        assert_eq!(
            codes,
            [
                "OUTSIDE_ROOT",
                "OUTSIDE_ROOT",
                "OUTSIDE_ROOT",
                "UNSUPPORTED_LSP_URI"
            ]
        );
    }

    #[test]
    fn canonical_root_uris_are_accepted() {
        let root = tempfile::tempdir().unwrap();
        let codec = UriCodec::new(root.path());
        let canonical = path_uri(&root.path().canonicalize().unwrap().join("a.rs"));
        assert_eq!(codec.relative(&canonical).unwrap(), "a.rs");
    }

    #[cfg(windows)]
    #[test]
    fn windows_uris_strip_verbatim_prefix_and_ignore_drive_case() {
        assert_eq!(
            path_uri(Path::new(r"\\?\C:\Projects\Code Weave\sample.py")),
            "file:///C:/Projects/Code%20Weave/sample.py"
        );
        assert_eq!(
            path_uri(Path::new(r"\\?\UNC\server\share\a.py")),
            "file://server/share/a.py"
        );
        let codec = UriCodec::new(Path::new(r"C:\Projects\Code Weave"));
        assert_eq!(
            codec
                .relative("file:///c%3A/projects/Code%20Weave/src/a.py")
                .unwrap(),
            "src/a.py"
        );
    }
}

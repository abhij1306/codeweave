use crate::error::{AppError, AppResult, ErrorCode};
use crate::security::validate_relative;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A line range bound to a workspace, path, and content hash. Handles travel
/// through clients, so decoding validates the range and path invariants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RangeHandleWire")]
pub struct RangeHandle {
    #[serde(rename = "w")]
    pub workspace_id: String,
    #[serde(rename = "p")]
    pub path: String,
    #[serde(rename = "a")]
    pub start_line: usize,
    #[serde(rename = "b")]
    pub end_line: usize,
    #[serde(rename = "h")]
    pub content_hash: String,
}

#[derive(Deserialize)]
struct RangeHandleWire {
    #[serde(rename = "w")]
    workspace_id: String,
    #[serde(rename = "p")]
    path: String,
    #[serde(rename = "a")]
    start_line: usize,
    #[serde(rename = "b")]
    end_line: usize,
    #[serde(rename = "h")]
    content_hash: String,
}

impl TryFrom<RangeHandleWire> for RangeHandle {
    type Error = String;

    fn try_from(wire: RangeHandleWire) -> Result<Self, Self::Error> {
        if wire.start_line > wire.end_line {
            return Err(format!(
                "range handle start line {} is after end line {}",
                wire.start_line, wire.end_line
            ));
        }
        let traverses = wire.path.split(['/', '\\']).any(|segment| segment == "..");
        if wire.path.is_empty() || traverses || validate_relative(&wire.path).is_err() {
            return Err("range handle path must be a workspace-relative path".to_owned());
        }
        Ok(Self {
            workspace_id: wire.workspace_id,
            path: wire.path,
            start_line: wire.start_line,
            end_line: wire.end_line,
            content_hash: wire.content_hash,
        })
    }
}

pub fn encode_handle(handle: &RangeHandle) -> AppResult<String> {
    let json = serde_json::to_vec(handle)?;
    Ok(format!("range:{}", URL_SAFE_NO_PAD.encode(json)))
}

pub fn decode_handle(input: &str) -> AppResult<RangeHandle> {
    let payload = input
        .strip_prefix("range:")
        .ok_or_else(|| AppError::new(ErrorCode::InvalidHandle, "Unsupported range handle"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| AppError::new(ErrorCode::InvalidHandle, error.to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::new(ErrorCode::InvalidHandle, error.to_string()))
}

pub fn content_hash(content: &str) -> String {
    hex(&Sha256::digest(content.as_bytes()))
}

/// Lowercase hexadecimal encoding shared by content hashes and snapshot ids.
pub(super) fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

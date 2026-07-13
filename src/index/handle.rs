use crate::model::{AppError, AppResult};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeHandle {
    #[serde(rename = "v", alias = "version")]
    pub version: u8,
    #[serde(rename = "w", alias = "workspace_id")]
    pub workspace_id: String,
    #[serde(rename = "p", alias = "path")]
    pub path: String,
    #[serde(rename = "a", alias = "start_line")]
    pub start_line: usize,
    #[serde(rename = "b", alias = "end_line")]
    pub end_line: usize,
    #[serde(rename = "h", alias = "content_hash")]
    pub content_hash: String,
}

pub fn encode_handle(handle: &RangeHandle) -> AppResult<String> {
    let json = serde_json::to_vec(handle)?;
    Ok(format!("range:v1:{}", URL_SAFE_NO_PAD.encode(json)))
}

pub fn decode_handle(input: &str) -> AppResult<RangeHandle> {
    let payload = input
        .strip_prefix("range:v1:")
        .ok_or_else(|| AppError::new("INVALID_HANDLE", "Unsupported range handle"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| AppError::new("INVALID_HANDLE", error.to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::new("INVALID_HANDLE", error.to_string()))
}

pub fn content_hash(content: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(content.as_bytes());
    let bytes = digest.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in bytes.iter() {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

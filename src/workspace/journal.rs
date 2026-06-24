use crate::model::AppResult;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;

pub(super) const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_JOURNAL_RECORDS: usize = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationRecord {
    pub mutation_id: String,
    #[serde(default)]
    pub session_id: String,
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub source: String,
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    pub generation: u64,
}

pub(super) fn rotate_journal_if_needed(path: &Path) -> AppResult<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() <= MAX_JOURNAL_BYTES {
        return Ok(());
    }
    let archive = path.with_file_name("mutations.previous.jsonl");
    if archive.exists() {
        fs::remove_file(&archive)?;
    }
    fs::rename(path, archive)?;
    Ok(())
}

pub(super) fn load_journal(path: &Path) -> VecDeque<MutationRecord> {
    let Ok(content) = fs::read_to_string(path) else {
        return VecDeque::new();
    };
    let mut journal: VecDeque<_> = content
        .lines()
        .filter_map(|line| serde_json::from_str::<MutationRecord>(line).ok())
        .collect();
    trim_journal(&mut journal);
    journal
}

pub(super) fn trim_journal(journal: &mut VecDeque<MutationRecord>) {
    while journal.len() > MAX_JOURNAL_RECORDS {
        journal.pop_front();
    }
}

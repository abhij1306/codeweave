use crate::model::AppResult;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

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

fn archive_paths(path: &Path) -> (PathBuf, PathBuf) {
    (
        path.with_file_name("mutations.previous.jsonl"),
        path.with_file_name("mutations.previous.backup.jsonl"),
    )
}

fn recover_interrupted_rotation(archive: &Path, backup: &Path) -> io::Result<()> {
    if !backup.exists() {
        return Ok(());
    }
    if archive.exists() {
        if let Err(error) = fs::remove_file(backup) {
            eprintln!(
                "journal rotation backup cleanup failed for {}: {error}",
                backup.display()
            );
        }
        Ok(())
    } else {
        fs::rename(backup, archive)
    }
}

fn rotate_journal(path: &Path, force: bool) -> AppResult<()> {
    let (archive, backup) = archive_paths(path);
    recover_interrupted_rotation(&archive, &backup)?;

    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if !force && metadata.len() <= MAX_JOURNAL_BYTES {
        return Ok(());
    }

    let displaced_archive = if archive.exists() {
        fs::rename(&archive, &backup)?;
        true
    } else {
        false
    };

    if let Err(rotation_error) = fs::rename(path, &archive) {
        if displaced_archive {
            if let Err(recovery_error) = fs::rename(&backup, &archive) {
                return Err(io::Error::new(
                    rotation_error.kind(),
                    format!(
                        "journal rotation failed: {rotation_error}; archive recovery failed: {recovery_error}; previous archive retained at {}",
                        backup.display()
                    ),
                )
                .into());
            }
        }
        return Err(rotation_error.into());
    }

    if displaced_archive {
        if let Err(error) = fs::remove_file(&backup) {
            eprintln!(
                "journal rotation backup cleanup failed for {}: {error}",
                backup.display()
            );
        }
    }
    Ok(())
}

pub(super) fn rotate_journal_if_needed(path: &Path) -> AppResult<()> {
    rotate_journal(path, false)
}

pub(super) fn rotate_journal_now(path: &Path) -> AppResult<()> {
    rotate_journal(path, true)
}

pub(super) fn open_journal(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
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

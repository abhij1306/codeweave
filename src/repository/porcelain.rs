//! `git status --porcelain=v2 -z --branch` output as a [`RepoStatus`].

use super::RepoStatus;
use std::collections::HashSet;

/// A Git path in canonical workspace form, or `None` when empty.
pub(super) fn normalized_path(path: &str) -> Option<String> {
    let path = crate::security::normalize_path_text(path);
    (!path.is_empty()).then(|| path.into_owned())
}

fn porcelain_path(record: &str, field_count: usize) -> Option<String> {
    record
        .splitn(field_count, ' ')
        .nth(field_count - 1)
        .and_then(normalized_path)
}

pub(super) fn parse_porcelain_v2(raw: &str) -> RepoStatus {
    let records: Vec<_> = raw
        .split('\0')
        .filter(|record| !record.is_empty())
        .collect();
    let mut status = RepoStatus {
        is_git: true,
        ..RepoStatus::default()
    };
    let mut dirty = HashSet::new();
    let mut staged: HashSet<String> = HashSet::new();
    let mut unstaged: HashSet<String> = HashSet::new();
    let mut untracked: HashSet<String> = HashSet::new();
    let mut index = 0usize;

    while index < records.len() {
        let record = records[index];
        if let Some(oid) = record.strip_prefix("# branch.oid ") {
            if oid != "(initial)" {
                status.head = oid.to_owned();
            }
        } else if let Some(branch) = record.strip_prefix("# branch.head ") {
            if branch != "(detached)" {
                status.branch = branch.to_owned();
            }
        } else {
            match record.as_bytes().first().copied() {
                Some(b'1') => {
                    // "1 XY sub mH mI mW hH hI path"; '.' = unmodified in that dimension
                    if let Some(path) = porcelain_path(record, 9) {
                        dirty.insert(path.clone());
                        let x = record.chars().nth(2).unwrap_or('.');
                        let y = record.chars().nth(3).unwrap_or('.');
                        if x != '.' {
                            staged.insert(path.clone());
                        }
                        if y != '.' {
                            unstaged.insert(path);
                        }
                    }
                }
                Some(b'2') => {
                    // "2 XY sub mH mI mW hH hI X score path"
                    if let Some(path) = porcelain_path(record, 10) {
                        dirty.insert(path.clone());
                        let x = record.chars().nth(2).unwrap_or('.');
                        let y = record.chars().nth(3).unwrap_or('.');
                        if x != '.' {
                            staged.insert(path.clone());
                        }
                        if y != '.' {
                            unstaged.insert(path);
                        }
                    }
                    if let Some(original) = records
                        .get(index + 1)
                        .and_then(|path| normalized_path(path))
                    {
                        dirty.insert(original);
                        index += 1;
                    }
                }
                Some(b'u') => {
                    // Unmerged: always in both staged and unstaged
                    if let Some(path) = porcelain_path(record, 11) {
                        dirty.insert(path.clone());
                        staged.insert(path.clone());
                        unstaged.insert(path);
                    }
                }
                Some(b'?') => {
                    if let Some(path) = record.strip_prefix("? ").and_then(normalized_path) {
                        dirty.insert(path.clone());
                        untracked.insert(path);
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }

    let partially_staged: HashSet<String> = staged.intersection(&unstaged).cloned().collect();
    status.dirty_files = dirty.into_iter().collect();
    status.dirty_files.sort();
    status.staged_files = staged.into_iter().collect();
    status.staged_files.sort();
    status.unstaged_files = unstaged.into_iter().collect();
    status.unstaged_files.sort();
    status.untracked_files = untracked.into_iter().collect();
    status.untracked_files.sort();
    status.partially_staged_files = partially_staged.into_iter().collect();
    status.partially_staged_files.sort();
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2_branch_and_dirty_paths() {
        let raw = concat!(
            "# branch.oid abc123\0",
            "# branch.head feature/status\0",
            "1 .M N... 100644 100644 100644 aaa bbb src/changed file.rs\0",
            "2 R. N... 100644 100644 100644 aaa bbb R100 src/new name.rs\0",
            "src/old name.rs\0",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc src/conflict.rs\0",
            "? src/untracked file.rs\0",
        );

        let status = parse_porcelain_v2(raw);

        assert!(status.is_git);
        assert_eq!(status.head, "abc123");
        assert_eq!(status.branch, "feature/status");
        assert_eq!(
            status.dirty_files,
            vec![
                "src/changed file.rs",
                "src/conflict.rs",
                "src/new name.rs",
                "src/old name.rs",
                "src/untracked file.rs",
            ]
        );
        assert_eq!(
            status.staged_files,
            vec!["src/conflict.rs", "src/new name.rs"]
        );
        assert_eq!(
            status.unstaged_files,
            vec!["src/changed file.rs", "src/conflict.rs"]
        );
        assert_eq!(status.untracked_files, vec!["src/untracked file.rs"]);
        assert_eq!(status.partially_staged_files, vec!["src/conflict.rs"]);
    }

    #[test]
    fn parses_unborn_detached_status_without_fake_values() {
        let status =
            parse_porcelain_v2("# branch.oid (initial)\0# branch.head (detached)\0? new.rs\0");

        assert!(status.is_git);
        assert!(status.head.is_empty());
        assert!(status.branch.is_empty());
        assert_eq!(status.dirty_files, vec!["new.rs"]);
        assert!(status.staged_files.is_empty());
        assert!(status.unstaged_files.is_empty());
        assert_eq!(status.untracked_files, vec!["new.rs"]);
        assert!(status.partially_staged_files.is_empty());
    }
}

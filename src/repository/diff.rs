//! Unified diffs as structured hunks.

use crate::index::content_hash;

/// A diff's text and the hunks parsed from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub text: String,
    pub hunks: Vec<DiffHunk>,
}

/// One `@@` hunk with its file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// Stable id: a hash of the path, both start lines and the hunk text.
    pub id: String,
    /// Destination path (`+++ b/<path>`).
    pub path: String,
    pub old: LineSpan,
    pub new: LineSpan,
    /// The file header followed by the hunk lines.
    pub text: String,
}

/// The lines one side of a hunk covers. A zero `count` covers no line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub start: usize,
    pub count: usize,
}

impl LineSpan {
    fn overlaps(self, start: usize, end: usize) -> bool {
        let last = self.start.saturating_add(self.count.saturating_sub(1));
        self.count > 0 && self.start <= end && last >= start
    }
}

impl DiffHunk {
    /// Whether either side of the hunk touches a line in `start..=end`.
    pub fn overlaps(&self, start: usize, end: usize) -> bool {
        self.old.overlaps(start, end) || self.new.overlaps(start, end)
    }
}

impl Diff {
    pub fn parse(text: String) -> Self {
        let hunks = parse_hunks(&text);
        Self { text, hunks }
    }

    /// The hunks that overlap every range in `ranges` and, when `ids` is not
    /// empty, whose id is listed, in diff order.
    pub fn select(self, ranges: &[(usize, usize)], ids: &[String]) -> Vec<DiffHunk> {
        self.hunks
            .into_iter()
            .filter(|hunk| ranges.iter().all(|&(start, end)| hunk.overlaps(start, end)))
            .filter(|hunk| ids.is_empty() || ids.iter().any(|id| id == &hunk.id))
            .collect()
    }
}

fn parse_span(value: &str) -> LineSpan {
    let value = value.trim_start_matches(['-', '+']);
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    LineSpan {
        start: start.parse().unwrap_or(0),
        count: count.parse().unwrap_or(1),
    }
}

struct OpenHunk {
    old: LineSpan,
    new: LineSpan,
    text: String,
}

fn parse_hunks(diff: &str) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut path = String::new();
    let mut header = String::new();
    let mut active: Option<OpenHunk> = None;
    let finish = |active: &mut Option<OpenHunk>, path: &str, hunks: &mut Vec<DiffHunk>| {
        if let Some(OpenHunk { old, new, text }) = active.take() {
            let id = content_hash(&format!("{path}\0{}\0{}\0{text}", old.start, new.start));
            hunks.push(DiffHunk {
                id,
                path: path.to_owned(),
                old,
                new,
                text,
            });
        }
    };
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            finish(&mut active, &path, &mut hunks);
            header.clear();
            path.clear();
            header.push_str(line);
            continue;
        }
        if let Some(value) = line.strip_prefix("+++ b/") {
            path = value.trim_end().to_owned();
        }
        if line.starts_with("@@ ") {
            finish(&mut active, &path, &mut hunks);
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let span = |index: usize| {
                fields
                    .get(index)
                    .map(|value| parse_span(value))
                    .unwrap_or(LineSpan { start: 0, count: 0 })
            };
            active = Some(OpenHunk {
                old: span(1),
                new: span(2),
                text: format!("{header}{line}"),
            });
        } else if let Some(open) = active.as_mut() {
            open.text.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    finish(&mut active, &path, &mut hunks);
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_FILES: &str = concat!(
        "diff --git a/a.rs b/a.rs\n",
        "index 1..2 100644\n",
        "--- a/a.rs\n",
        "+++ b/a.rs\n",
        "@@ -2,1 +2,1 @@\n",
        "-old two\n",
        "+new two\n",
        "@@ -40 +40,0 @@\n",
        "-gone forty\n",
        "diff --git a/b.rs b/b.rs\n",
        "--- /dev/null\n",
        "+++ b/b.rs\n",
        "@@ -0,0 +1,2 @@\n",
        "+one\n",
        "+two\n",
    );

    fn spans(hunks: &[DiffHunk]) -> Vec<(&str, usize, usize, usize, usize)> {
        hunks
            .iter()
            .map(|hunk| {
                (
                    hunk.path.as_str(),
                    hunk.old.start,
                    hunk.old.count,
                    hunk.new.start,
                    hunk.new.count,
                )
            })
            .collect()
    }

    #[test]
    fn hunks_carry_their_path_spans_and_file_header() {
        let diff = Diff::parse(TWO_FILES.to_owned());

        assert_eq!(
            spans(&diff.hunks),
            [
                ("a.rs", 2, 1, 2, 1),
                ("a.rs", 40, 1, 40, 0),
                ("b.rs", 0, 0, 1, 2)
            ]
        );
        assert_eq!(
            diff.hunks[1].text,
            "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -40 +40,0 @@\n-gone forty\n"
        );
        assert_eq!(diff.text, TWO_FILES);
    }

    #[test]
    fn selection_requires_every_range_and_a_listed_id() {
        let diff = Diff::parse(TWO_FILES.to_owned());
        let second = diff.hunks[1].id.clone();

        assert_eq!(
            spans(&diff.clone().select(&[(1, 3)], &[])),
            [("a.rs", 2, 1, 2, 1), ("b.rs", 0, 0, 1, 2)]
        );
        assert_eq!(
            spans(&diff.clone().select(&[(1, 3), (2, 2)], &[])),
            [("a.rs", 2, 1, 2, 1), ("b.rs", 0, 0, 1, 2)]
        );
        assert!(diff.clone().select(&[(1, 1), (40, 40)], &[]).is_empty());
        assert_eq!(
            spans(&diff.select(&[], &[second])),
            [("a.rs", 40, 1, 40, 0)]
        );
    }

    #[test]
    fn a_zero_length_side_overlaps_no_line() {
        let diff = Diff::parse(TWO_FILES.to_owned());
        let deletion = &diff.hunks[1];
        assert!(deletion.overlaps(40, 40));
        assert!(!deletion.overlaps(41, 41));
        let addition = &diff.hunks[2];
        assert!(!addition.overlaps(0, 0));
        assert!(addition.overlaps(2, 9));
    }
}

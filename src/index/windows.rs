//! Windowed, per-file-capped, round-robin interleaved results, shared by text
//! search and reference search.
//!
//! Hits in one file are widened by context lines into windows, and adjacent
//! windows merge. Each file contributes at most a fixed number of windows, and
//! files then take turns so one noisy file cannot fill the whole result.

use crate::model::AppResult;
use std::collections::VecDeque;

/// Hits of one file merged into a line range.
#[derive(Debug)]
pub(crate) struct Window<T> {
    /// First line of the first hit.
    pub(crate) line: usize,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) hits: Vec<T>,
}

/// Collects one file's hits, in line order, into windows.
pub(crate) struct WindowBuilder<T> {
    context_lines: usize,
    line_count: usize,
    max_hits_per_window: usize,
    windows: Vec<Window<T>>,
}

impl<T> WindowBuilder<T> {
    pub(crate) fn new(context_lines: usize, line_count: usize, max_hits_per_window: usize) -> Self {
        Self {
            context_lines,
            line_count,
            max_hits_per_window,
            windows: Vec::new(),
        }
    }

    /// Add a hit spanning `start_line..=end_line`. It joins the previous
    /// window when the context ranges touch and that window has room.
    pub(crate) fn push(&mut self, start_line: usize, end_line: usize, hit: T) {
        let start = start_line.saturating_sub(self.context_lines).max(1);
        let end = end_line
            .saturating_add(self.context_lines)
            .min(self.line_count);
        if let Some(previous) = self.windows.last_mut() {
            if start <= previous.end_line.saturating_add(1)
                && previous.hits.len() < self.max_hits_per_window
            {
                previous.end_line = previous.end_line.max(end);
                previous.hits.push(hit);
                return;
            }
        }
        self.windows.push(Window {
            line: start_line,
            start_line: start,
            end_line: end,
            hits: vec![hit],
        });
    }

    pub(crate) fn finish(self) -> Vec<Window<T>> {
        self.windows
    }
}

/// Per-file result groups awaiting interleaving.
pub(crate) struct FileGroups<T> {
    groups: Vec<VecDeque<T>>,
    total_windows: usize,
}

/// Interleaved results, and whether windows were left out.
pub(crate) struct Interleaved<T> {
    pub(crate) results: Vec<T>,
    pub(crate) truncated: bool,
}

impl<T> FileGroups<T> {
    pub(crate) fn new() -> Self {
        Self {
            groups: Vec::new(),
            total_windows: 0,
        }
    }

    /// A group that is not counted as a matched window, such as a declaration
    /// added ahead of the matches.
    pub(crate) fn push_extra(&mut self, result: T) {
        self.groups.push(VecDeque::from([result]));
    }

    /// Add one file's windows, rendering at most `per_file_limit` of them.
    pub(crate) fn push_file<W>(
        &mut self,
        windows: Vec<W>,
        per_file_limit: usize,
        render: impl FnMut(W) -> AppResult<T>,
    ) -> AppResult<()> {
        self.total_windows += windows.len();
        let group = windows
            .into_iter()
            .take(per_file_limit)
            .map(render)
            .collect::<AppResult<VecDeque<_>>>()?;
        if !group.is_empty() {
            self.groups.push(group);
        }
        Ok(())
    }

    /// Order groups by the key of their first result; equal keys keep their order.
    pub(crate) fn sort_by_first<K: Ord>(&mut self, mut key: impl FnMut(&T) -> K) {
        self.groups.sort_by_key(|group| group.front().map(&mut key));
    }

    /// Take one result from each group in turn until `limit` results are taken.
    pub(crate) fn interleave(mut self, limit: usize) -> Interleaved<T> {
        let mut results = Vec::new();
        while results.len() < limit {
            let mut added = false;
            for group in &mut self.groups {
                if results.len() >= limit {
                    break;
                }
                if let Some(result) = group.pop_front() {
                    results.push(result);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        Interleaved {
            truncated: self.total_windows > results.len(),
            results,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_hits_merge_until_the_window_is_full() {
        let mut builder = WindowBuilder::new(1, 20, 2);
        for line in [3, 4, 5, 12] {
            builder.push(line, line, line);
        }

        let windows: Vec<_> = builder
            .finish()
            .into_iter()
            .map(|window| (window.line, window.start_line, window.end_line, window.hits))
            .collect();

        assert_eq!(
            windows,
            vec![
                (3, 2, 5, vec![3, 4]),
                (5, 4, 6, vec![5]),
                (12, 11, 13, vec![12]),
            ]
        );
    }

    #[test]
    fn files_take_turns_and_truncation_counts_capped_windows() {
        let mut groups = FileGroups::new();
        groups.push_file(vec!["a1", "a2", "a3"], 2, Ok).unwrap();
        groups.push_file(vec!["b1"], 2, Ok).unwrap();

        let interleaved = groups.interleave(10);

        assert_eq!(interleaved.results, vec!["a1", "b1", "a2"]);
        assert!(interleaved.truncated);
    }
}

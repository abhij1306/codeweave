use super::path_filter::PathFilterSet;
use super::query::{FilenameQuery, SearchScope, SymbolQuery, TextQuery, TextSyntax};
use super::windows::{FileGroups, WindowBuilder};
use super::{encode_handle, slice_lines, CodeIndex, FileEntry, RangeHandle};
use crate::error::{AppError, AppResult, ErrorCode};
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Text windows per file when the scope is not a single path.
const TEXT_WINDOWS_PER_FILE: usize = 8;

#[derive(Serialize)]
struct TextMatch<'a> {
    path: &'a str,
    line: usize,
    start_line: usize,
    end_line: usize,
    preview: String,
    handle: String,
    match_type: &'static str,
    score: f64,
    reason_codes: [&'static str; 1],
}

impl CodeIndex {
    /// Literal or regex search over file content, one result per window of
    /// matching lines.
    pub fn search_text(&self, scope: &SearchScope<'_>, query: &TextQuery) -> AppResult<Value> {
        if query.pattern.is_empty() {
            return Err(AppError::invalid("query is required for text search"));
        }
        let matcher = LineMatcher::new(query)?;
        let path_filters = PathFilterSet::new(scope.paths);
        let mut files = self
            .files()
            .filter(|file| path_filters.allows(file.path()))
            .filter(|file| matcher.may_match(file))
            .collect::<Vec<_>>();
        files.sort_unstable_by(|left, right| left.path().cmp(right.path()));

        let limit = scope.limit();
        let per_file_limit = if path_filters.len() == 1 {
            limit
        } else {
            limit.min(TEXT_WINDOWS_PER_FILE)
        };
        let (match_type, reason_code) = match query.syntax {
            TextSyntax::Literal => ("literal", "literal_match"),
            TextSyntax::Regex => ("regex", "regex_match"),
        };
        let mut groups = FileGroups::new();
        for file in files {
            let mut builder =
                WindowBuilder::new(query.context_lines, file.line_count(), usize::MAX);
            for (index, line) in matcher.haystack(file).lines().enumerate() {
                if matcher.matches(line) {
                    builder.push(index + 1, index + 1, ());
                }
            }
            groups.push_file(builder.finish(), per_file_limit, |window| {
                Ok(TextMatch {
                    path: file.path(),
                    line: window.line,
                    start_line: window.start_line,
                    end_line: window.end_line,
                    preview: slice_lines(file.content(), window.start_line, window.end_line),
                    handle: encode_handle(&RangeHandle {
                        workspace_id: scope.workspace_id.to_owned(),
                        path: file.path().to_owned(),
                        start_line: window.start_line,
                        end_line: window.end_line,
                        content_hash: file.hash().to_owned(),
                    })?,
                    match_type,
                    score: 1.0,
                    reason_codes: [reason_code],
                })
            })?;
        }
        let interleaved = groups.interleave(limit);
        Ok(json!({
            "mode": match_type,
            "snapshot_id": scope.snapshot_id,
            "result_count": interleaved.results.len(),
            "truncated": interleaved.truncated,
            "results": interleaved.results
        }))
    }

    /// Files whose path contains the name, or matches it as a glob.
    pub fn search_filenames(
        &self,
        scope: &SearchScope<'_>,
        query: &FilenameQuery,
    ) -> AppResult<Value> {
        let path_filters = PathFilterSet::new(scope.paths);
        let matcher = FilenameMatcher::new(&query.name, query.case_sensitive)?;
        let mut matched: Vec<&str> = self
            .files()
            .filter(|file| path_filters.allows(file.path()))
            .filter(|file| matcher.matches(file))
            .map(FileEntry::path)
            .collect();
        matched.sort_unstable();
        let total = matched.len();
        let paths: Vec<_> = matched
            .into_iter()
            .take(scope.limit())
            .map(|path| {
                json!({
                    "path": path,
                    "match_type": matcher.semantics(),
                    "score": 1.0,
                    "reason_codes": ["filename_match"]
                })
            })
            .collect();
        Ok(json!({
            "mode": "filename",
            "match_semantics": matcher.semantics(),
            "snapshot_id": scope.snapshot_id,
            "result_count": paths.len(),
            "truncated": total > paths.len(),
            "results": paths
        }))
    }

    /// Declarations whose name contains the query, exact matches first.
    pub fn search_symbols(&self, scope: &SearchScope<'_>, query: &SymbolQuery) -> AppResult<Value> {
        let path_filters = PathFilterSet::new(scope.paths);
        let needle = query.name.to_ascii_lowercase();
        let mut results = Vec::new();
        for name in self.symbol_names() {
            let normalized_name = name.to_ascii_lowercase();
            if !normalized_name.contains(&needle) {
                continue;
            }
            let rank = if normalized_name == needle {
                SymbolRank::Exact
            } else if normalized_name.starts_with(&needle) {
                SymbolRank::Prefix
            } else {
                SymbolRank::Contains
            };
            for (file, symbol) in self.indexed_symbols(name) {
                if !path_filters.allows(file.path()) {
                    continue;
                }
                let handle = encode_handle(&RangeHandle {
                    workspace_id: scope.workspace_id.to_owned(),
                    path: file.path().to_owned(),
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    content_hash: file.hash().to_owned(),
                })?;
                results.push((
                    rank,
                    file.path(),
                    symbol.start_line,
                    json!({
                        "path": file.path(),
                        "symbol": symbol,
                        "handle": handle,
                        "match_type": rank.match_type(),
                        "score": rank.score(),
                        "reason_codes": [rank.reason_code()]
                    }),
                ));
            }
        }
        results.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        let results: Vec<_> = results
            .into_iter()
            .take(scope.limit())
            .map(|(_, _, _, value)| value)
            .collect();
        Ok(json!({
            "mode": "symbol",
            "snapshot_id": scope.snapshot_id,
            "result_count": results.len(),
            "results": results
        }))
    }

    /// The symbol outline of one indexed file.
    pub fn outline(&self, path: &str) -> AppResult<Value> {
        let file = self.get(path).ok_or_else(|| {
            AppError::with_details(
                ErrorCode::PathNotIndexed,
                "Outline path is not indexed",
                json!({"path": path}),
            )
        })?;
        Ok(json!({
            "mode": "outline",
            "path": file.path(),
            "hash": file.hash(),
            "symbols": file.symbols()
        }))
    }

    /// Indexed directories with file counts and languages, in path order.
    pub fn repo_map(&self, scope: &SearchScope<'_>) -> Value {
        let path_filters = PathFilterSet::new(scope.paths);
        let mut directories: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
        let mut scoped_file_count = 0usize;
        for file in self.files() {
            if !path_filters.allows(file.path()) {
                continue;
            }
            scoped_file_count += 1;
            let directory = file
                .path()
                .rsplit_once('/')
                .map(|(dir, _)| dir)
                .unwrap_or(".");
            let value = directories.entry(directory).or_default();
            value.0 += 1;
            value.1.insert(file.language());
        }
        let entries: Vec<_> = directories
            .into_iter()
            .take(scope.limit())
            .map(|(path, (files, languages))| {
                json!({"path": path, "files": files, "languages": languages})
            })
            .collect();
        json!({
            "mode": "repo_map",
            "snapshot_id": scope.snapshot_id,
            "directories": entries,
            "file_count": scoped_file_count,
            "total_file_count": self.file_count(),
            "scope_applied": path_filters.len() > 0
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SymbolRank {
    Exact,
    Prefix,
    Contains,
}

impl SymbolRank {
    fn match_type(self) -> &'static str {
        match self {
            Self::Exact => "exact_symbol",
            Self::Prefix => "prefix_symbol",
            Self::Contains => "contains_symbol",
        }
    }

    fn score(self) -> f64 {
        match self {
            Self::Exact => 1.0,
            Self::Prefix => 0.8,
            Self::Contains => 0.6,
        }
    }

    fn reason_code(self) -> &'static str {
        match self {
            Self::Exact => "exact_symbol",
            Self::Prefix | Self::Contains => "symbol_match",
        }
    }
}

/// Line matching for one text query.
enum LineMatcher {
    /// Case-sensitive literal matched against the original content.
    Exact(String),
    /// Lowercased literal matched against the lowercased content.
    Folded(String),
    Regex(Regex),
}

impl LineMatcher {
    fn new(query: &TextQuery) -> AppResult<Self> {
        Ok(match query.syntax {
            TextSyntax::Literal if query.case_sensitive => Self::Exact(query.pattern.clone()),
            TextSyntax::Literal => Self::Folded(query.pattern.to_ascii_lowercase()),
            TextSyntax::Regex => Self::Regex(
                RegexBuilder::new(&query.pattern)
                    .case_insensitive(!query.case_sensitive)
                    .build()
                    .map_err(|error| {
                        AppError::with_details(
                            ErrorCode::InvalidRegex,
                            error.to_string(),
                            json!({"query_length": query.pattern.len()}),
                        )
                    })?,
            ),
        })
    }

    /// Exact file-level prefilter: a literal can only match a line of a file
    /// that contains it, so skipping other files never loses a match.
    fn may_match(&self, file: &FileEntry) -> bool {
        match self {
            Self::Exact(needle) | Self::Folded(needle) => {
                self.haystack(file).contains(needle.as_str())
            }
            Self::Regex(_) => true,
        }
    }

    fn haystack<'a>(&self, file: &'a FileEntry) -> &'a str {
        match self {
            Self::Folded(_) => file.search_content(),
            Self::Exact(_) | Self::Regex(_) => file.content(),
        }
    }

    fn matches(&self, line: &str) -> bool {
        match self {
            Self::Exact(needle) | Self::Folded(needle) => line.contains(needle.as_str()),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

enum FilenameMatcher {
    Substring {
        needle: String,
        case_sensitive: bool,
    },
    Glob {
        regex: Regex,
    },
}

impl FilenameMatcher {
    fn new(query: &str, case_sensitive: bool) -> AppResult<Self> {
        if has_glob_wildcards(query) {
            let pattern = glob_pattern_to_regex(query);
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(AppError::internal)?;
            return Ok(Self::Glob { regex });
        }
        Ok(Self::Substring {
            needle: if case_sensitive {
                query.to_owned()
            } else {
                query.to_ascii_lowercase()
            },
            case_sensitive,
        })
    }

    fn matches(&self, file: &FileEntry) -> bool {
        match self {
            Self::Substring {
                needle,
                case_sensitive,
            } => {
                let hay = if *case_sensitive {
                    file.path()
                } else {
                    file.path_lower()
                };
                hay.contains(needle.as_str())
            }
            Self::Glob { regex } => regex.is_match(file.path()),
        }
    }

    fn semantics(&self) -> &'static str {
        match self {
            Self::Substring { .. } => "substring",
            Self::Glob { .. } => "glob",
        }
    }
}

fn has_glob_wildcards(query: &str) -> bool {
    query.contains('*') || query.contains('?')
}

fn glob_pattern_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');
    regex
}

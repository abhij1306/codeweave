use super::FileEntry;
use crate::symbols::Symbol;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

pub(super) fn classify_document(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();
    let basename = segments.last().copied().unwrap_or(lower.as_str());
    if basename == "agents.md" || basename == "claude.md" {
        return "instruction".to_owned();
    }
    if segments
        .iter()
        .any(|segment| matches!(*segment, "test" | "tests" | "__tests__"))
        || basename.starts_with("test_")
        || basename.ends_with("_test.rs")
        || basename.ends_with(".test.ts")
        || basename.ends_with(".spec.ts")
        || basename.ends_with(".test.tsx")
        || basename.ends_with(".spec.tsx")
        || basename.ends_with(".test.js")
        || basename.ends_with(".spec.js")
        || basename.ends_with(".test.jsx")
        || basename.ends_with(".spec.jsx")
    {
        return "test".to_owned();
    }
    if (segments.contains(&"evidence") || segments.contains(&"runtime"))
        && (basename.ends_with(".json") || basename.ends_with(".log"))
    {
        return "runtime_evidence".to_owned();
    }
    if segments.iter().any(|segment| {
        matches!(
            *segment,
            "artifact" | "artifacts" | "fixture" | "fixtures" | "recording" | "recordings"
        )
    }) || basename.ends_with(".recording.json")
        || basename.ends_with(".fixture.json")
        || basename.ends_with(".fixtures.json")
        || basename.ends_with(".artifact.json")
        || basename.ends_with(".artifacts.json")
    {
        return "artifact".to_owned();
    }
    if basename.ends_with(".log") {
        return "log".to_owned();
    }
    "source".to_owned()
}

pub(super) fn query_terms(query: &str) -> Vec<String> {
    static TERM_REGEX: OnceLock<Regex> = OnceLock::new();
    static STOP_WORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let regex = TERM_REGEX.get_or_init(|| {
        Regex::new(r"[A-Za-z_][A-Za-z0-9_.-]{1,}").expect("valid query term regex")
    });
    let stop = STOP_WORDS.get_or_init(|| {
        [
            "a", "an", "as", "at", "be", "by", "for", "from", "in", "into", "is", "it", "of", "on",
            "or", "the", "this", "that", "to", "we", "with", "you", "when", "where", "what", "how",
            "why", "find", "focus", "include", "fix", "add", "change", "update", "code", "file",
            "files", "result", "results", "source", "tests", "tool", "tools",
        ]
        .into_iter()
        .collect()
    });
    let mut terms: Vec<String> = regex
        .find_iter(query)
        .map(|m| {
            m.as_str()
                .trim_matches(|c: char| c == '`' || c == '.' || c == '-')
                .to_ascii_lowercase()
        })
        .filter(|term| term.len() > 1 && !stop.contains(term.as_str()))
        .collect();
    terms.sort();
    terms.dedup();
    terms
}

pub(super) fn build_indexed_terms(
    search_content: &str,
    path_lower: &str,
    symbols: &[Symbol],
) -> Vec<String> {
    let mut terms = query_terms(search_content);
    terms.extend(query_terms(path_lower));
    for symbol in symbols {
        terms.extend(query_terms(&symbol.name));
    }
    let compound_parts: Vec<_> = terms
        .iter()
        .flat_map(|term| {
            let separated: String = term
                .chars()
                .map(|ch| {
                    if matches!(ch, '_' | '.' | '-') {
                        ' '
                    } else {
                        ch
                    }
                })
                .collect();
            query_terms(&separated)
        })
        .collect();
    terms.extend(compound_parts);
    terms.sort();
    terms.dedup();
    terms
}

pub(super) fn normalize_entry(entry: &mut FileEntry) {
    if entry.search_content.is_empty() {
        entry.search_content = entry.content.to_ascii_lowercase();
    }
    if entry.path_lower.is_empty() {
        entry.path_lower = entry.path.to_ascii_lowercase();
    }
    if entry.line_count == 0 {
        entry.line_count = entry.content.lines().count().max(1);
    }
    if entry.line_starts.is_empty() {
        entry.line_starts = super::lines::line_starts(&entry.content);
    }
    if entry.indexed_terms.is_empty() {
        entry.indexed_terms =
            build_indexed_terms(&entry.search_content, &entry.path_lower, &entry.symbols);
    }
}

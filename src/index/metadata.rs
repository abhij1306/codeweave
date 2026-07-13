use super::FileEntry;
use crate::symbols::Symbol;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

pub(super) fn low_signal_context_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "license"
            | "license.md"
            | "license.txt"
    )
}

pub(super) fn evidence_allowed(document_type: &str, evidence: &[String]) -> bool {
    if evidence.is_empty() {
        return true;
    }
    evidence.iter().any(|item| match item.as_str() {
        "source" => document_type == "source",
        "tests" => document_type == "test",
        "artifacts" => matches!(document_type, "runtime_evidence" | "artifact" | "log"),
        "changes" => false,
        "instructions" => document_type == "instruction",
        _ => false,
    })
}

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

pub(super) fn classify_lifecycle(path: &str, content: &str) -> String {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    if !normalized.starts_with("docs/plans/") {
        return "current".to_owned();
    }
    let header = content
        .chars()
        .take(8_192)
        .collect::<String>()
        .to_ascii_uppercase();
    if ["SUPERSEDED", "HISTORICAL", "COMPLETE"]
        .iter()
        .any(|marker| header.contains(marker))
    {
        "historical_plan".to_owned()
    } else {
        "active_plan".to_owned()
    }
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

pub(super) fn compact_reason_codes(mut reasons: Vec<String>) -> Vec<String> {
    const PRIORITY: &[&str] = &[
        "exact_symbol",
        "exact_phrase",
        "full_term_coverage",
        "required_term",
        "path_match",
        "runtime_evidence",
        "dirty_file",
        "recent_mutation",
        "symbol_match",
    ];
    reasons.sort();
    reasons.dedup();
    let mut compact = Vec::new();
    for preferred in PRIORITY {
        if reasons.iter().any(|reason| reason == preferred) {
            compact.push((*preferred).to_owned());
        }
        if compact.len() == 3 {
            break;
        }
    }
    compact
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
    if entry.chunks.is_empty() {
        entry.chunks = super::chunks::build_chunks(&entry.content, &entry.symbols);
    }
    if entry.path_tf.is_empty() {
        entry.path_tf = super::chunks::path_field(&entry.path_lower);
    }
}

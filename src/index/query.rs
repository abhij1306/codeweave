//! Typed queries accepted by the index.

/// Where results come from and how many may be returned.
#[derive(Debug, Clone, Copy)]
pub struct SearchScope<'a> {
    pub workspace_id: &'a str,
    pub snapshot_id: &'a str,
    /// Strict workspace-relative path scope; empty means the whole index.
    pub paths: &'a [String],
    pub max_results: usize,
}

impl SearchScope<'_> {
    /// The result limit; at least one result is always allowed.
    pub fn limit(&self) -> usize {
        self.max_results.max(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSyntax {
    Literal,
    Regex,
}

impl TextSyntax {
    pub const ALL: [Self; 2] = [Self::Literal, Self::Regex];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Regex => "regex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|syntax| syntax.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextQuery {
    pub pattern: String,
    pub syntax: TextSyntax,
    pub case_sensitive: bool,
    pub context_lines: usize,
}

/// A filename substring, or a glob when it contains `*` or `?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilenameQuery {
    pub name: String,
    pub case_sensitive: bool,
}

/// A case-insensitive symbol-name substring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolQuery {
    pub name: String,
}

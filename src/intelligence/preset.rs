use crate::symbols::language_name;
use std::path::Path;

/// One supported language-server family. The preset owns every per-language
/// fact: its configuration key, LSP language id, default command, and which
/// Tree-sitter language names it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LspPreset {
    Rust,
    Python,
    TypeScript,
}

impl LspPreset {
    pub(crate) const ALL: [Self; 3] = [Self::Rust, Self::Python, Self::TypeScript];

    /// The preset whose server handles `path`, judged by its language name.
    pub(crate) fn for_path(path: &Path) -> Option<Self> {
        let language = language_name(path);
        Self::ALL
            .into_iter()
            .find(|preset| preset.languages().contains(&language))
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }

    pub(crate) fn language_id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }

    /// Language names (as reported by `symbols::language_name`) this server serves.
    fn languages(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rust"],
            Self::Python => &["python"],
            Self::TypeScript => &["typescript", "tsx", "javascript"],
        }
    }

    pub(crate) fn default_command(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Python => "basedpyright-langserver",
            Self::TypeScript => "typescript-language-server",
        }
    }

    pub(super) fn default_args(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[],
            Self::Python | Self::TypeScript => &["--stdio"],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_route_to_the_preset_that_serves_their_language() {
        let routed = ["a.rs", "b.py", "c.pyi", "d.ts", "e.tsx", "f.mjs", "g.go"]
            .map(|name| LspPreset::for_path(Path::new(name)));
        assert_eq!(
            routed,
            [
                Some(LspPreset::Rust),
                Some(LspPreset::Python),
                Some(LspPreset::Python),
                Some(LspPreset::TypeScript),
                Some(LspPreset::TypeScript),
                Some(LspPreset::TypeScript),
                None,
            ]
        );
    }
}

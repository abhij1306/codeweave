#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LspPreset {
    Rust,
    Python,
    TypeScript,
}

impl LspPreset {
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

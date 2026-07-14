//! CodeWeave application library. The binary is a thin composition root; all
//! repository, protocol, execution, and workspace behavior is compiled once
//! here and is shared by tests and evaluation code.

pub mod bash;
pub mod contracts;
pub mod index;
pub mod intelligence;
pub mod manager;
pub mod model;
pub mod process_runtime;
pub mod reference_service;
pub mod repository;
pub mod retrieval;
pub mod security;
pub mod symbols;
pub mod tools;
pub mod workspace;

#[cfg(test)]
pub(crate) fn test_bash_executable() -> String {
    #[cfg(windows)]
    {
        for root in [
            std::env::var_os("ProgramW6432"),
            std::env::var_os("ProgramFiles"),
        ]
        .into_iter()
        .flatten()
        {
            let candidate = std::path::PathBuf::from(root)
                .join("Git")
                .join("bin")
                .join("bash.exe");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "bash".to_owned()
}

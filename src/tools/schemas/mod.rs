//! Per-domain input-schema builders. Each function returns the flat draft-07
//! `inputSchema` value for one tool; the registry references them by function
//! pointer so there is exactly one definition of each schema.

pub mod bash;
pub mod edits;
pub mod git;
pub mod retrieval;
pub mod workspace;

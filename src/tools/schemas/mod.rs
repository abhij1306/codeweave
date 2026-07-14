//! Per-domain draft-07 input-schema builders. Top-level tool requests remain
//! flat; edit transaction items use the operation-specific union published by
//! `edits`. The registry references each builder by function pointer so there
//! is exactly one definition of every schema.

pub mod bash;
pub mod edits;
pub mod git;
pub mod intelligence;
pub mod retrieval;
pub mod workspace;

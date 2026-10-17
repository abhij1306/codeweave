//! Typed requests for every public tool.
//!
//! Each tool has one request type. It derives the tool's published input
//! schema (`contract::input_schema`), the field contract checked at the
//! boundary (`Contract`) and the typed value its handler receives. Raw JSON
//! exists only between the transport and `Contract::parse`.

mod contract;

pub mod bash;
pub mod edit;
pub mod git;
pub mod intelligence;
pub mod retrieval;
pub mod workspace;

pub use contract::{input_schema, Contract, Rejection};

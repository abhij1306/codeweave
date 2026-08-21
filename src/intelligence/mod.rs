mod normalize;
mod preset;
mod protocol;
mod service;
mod sync;
mod transport;
mod worker;
mod workspace_edit;

pub use service::IntelligenceService;

#[cfg(test)]
mod tests;

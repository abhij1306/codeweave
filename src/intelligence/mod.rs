mod normalize;
mod protocol;
mod service;
mod sync;
mod worker;
mod workspace_edit;

pub use service::IntelligenceService;

#[cfg(test)]
mod tests;

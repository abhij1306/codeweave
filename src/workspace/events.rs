use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;

const MAX_MUTATION_EVENTS: usize = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct MutationRecord {
    pub mutation_id: String,
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub source: String,
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    pub generation: u64,
}

impl MutationRecord {
    pub(super) fn new_id() -> String {
        format!("mut_{}", uuid::Uuid::new_v4().simple())
    }
}

pub(super) fn append_events(events: &mut VecDeque<MutationRecord>, records: &[MutationRecord]) {
    events.extend(records.iter().cloned());
    while events.len() > MAX_MUTATION_EVENTS {
        events.pop_front();
    }
}

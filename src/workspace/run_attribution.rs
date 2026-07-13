use super::util::{summarize_changed_paths, ChangedPathSummary};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub(super) struct RunBaseline {
    pub(super) generation: u64,
    pub(super) dirty_files: HashSet<String>,
    completion: Option<RunCompletion>,
    frozen: Option<FrozenRunAttribution>,
}

#[derive(Debug, Clone)]
struct RunCompletion {
    ended_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct FrozenRunAttribution {
    generation: u64,
    changed: ChangedPathSummary,
}

#[derive(Default)]
struct RunAttributionState {
    baselines: HashMap<String, RunBaseline>,
    pending_completions: HashMap<String, RunCompletion>,
}

/// Owns all Bash-run attribution state behind one mutex.
///
/// Callers must capture any other workspace state needed by `observe` before
/// invoking it. The observation callback executes while this mutex is held so a
/// terminal result can be frozen exactly once without acquiring a second
/// attribution lock.
pub(super) struct RunAttribution {
    state: Mutex<RunAttributionState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttributionSnapshot {
    pub(super) start_generation: u64,
    pub(super) attribution_generation: u64,
    pub(super) changed: ChangedPathSummary,
}

impl RunBaseline {
    pub(super) fn new(generation: u64, dirty_files: HashSet<String>) -> Self {
        Self {
            generation,
            dirty_files,
            completion: None,
            frozen: None,
        }
    }
}

impl RunAttribution {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(RunAttributionState::default()),
        }
    }

    pub(super) fn record_completion(&self, run_id: &str, ended_at: DateTime<Utc>) {
        let completion = RunCompletion { ended_at };
        let mut state = self.state.lock();
        if let Some(baseline) = state.baselines.get_mut(run_id) {
            if baseline.completion.is_none() {
                baseline.completion = Some(completion);
            }
        } else {
            state
                .pending_completions
                .insert(run_id.to_owned(), completion);
        }
    }

    pub(super) fn start_run(&self, run_id: &str, generation: u64, dirty_files: HashSet<String>) {
        let mut state = self.state.lock();
        let completion = state.pending_completions.remove(run_id);
        let mut baseline = RunBaseline::new(generation, dirty_files);
        baseline.completion = completion;
        state.baselines.insert(run_id.to_owned(), baseline);
    }

    pub(super) fn evict_runs(&self, run_ids: &[String]) {
        let mut state = self.state.lock();
        for run_id in run_ids {
            state.baselines.remove(run_id);
            state.pending_completions.remove(run_id);
        }
    }

    pub(super) fn observe<F>(
        &self,
        run_id: &str,
        current_generation: u64,
        current_dirty: HashSet<String>,
        terminal: bool,
        result_ended_at: Option<DateTime<Utc>>,
        changed_paths: F,
    ) -> AttributionSnapshot
    where
        F: FnOnce(&RunBaseline, u64, Option<&DateTime<Utc>>, &HashSet<String>) -> HashSet<String>,
    {
        let mut state = self.state.lock();
        let pending_completion = state.pending_completions.remove(run_id);
        let baseline = state
            .baselines
            .entry(run_id.to_owned())
            .or_insert_with(|| RunBaseline::new(current_generation, current_dirty.clone()));
        if baseline.completion.is_none() {
            baseline.completion = pending_completion;
        }
        if terminal && baseline.completion.is_none() {
            if let Some(ended_at) = result_ended_at {
                baseline.completion = Some(RunCompletion { ended_at });
            }
        }
        if let Some(frozen) = &baseline.frozen {
            return AttributionSnapshot {
                start_generation: baseline.generation,
                attribution_generation: frozen.generation,
                changed: frozen.changed.clone(),
            };
        }

        let attribution_generation = current_generation;
        let ended_at = if terminal {
            let completion = baseline
                .completion
                .clone()
                .unwrap_or_else(|| RunCompletion {
                    ended_at: Utc::now(),
                });
            Some(completion.ended_at)
        } else {
            None
        };
        let changed = summarize_changed_paths(changed_paths(
            baseline,
            attribution_generation,
            ended_at.as_ref(),
            &current_dirty,
        ));
        if terminal {
            baseline.frozen = Some(FrozenRunAttribution {
                generation: attribution_generation,
                changed: changed.clone(),
            });
        }
        AttributionSnapshot {
            start_generation: baseline.generation,
            attribution_generation,
            changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn completion_recorded_before_start_uses_post_reconcile_generation() {
        let attribution = RunAttribution::new();
        let ended_at = Utc::now();
        attribution.record_completion("run", ended_at);
        attribution.start_run("run", 2, HashSet::new());

        let snapshot = attribution.observe(
            "run",
            7,
            HashSet::new(),
            true,
            None,
            |_, generation, completion, _| {
                assert_eq!(generation, 7);
                assert_eq!(completion, Some(&ended_at));
                HashSet::new()
            },
        );

        assert_eq!(snapshot.start_generation, 2);
        assert_eq!(snapshot.attribution_generation, 7);
    }

    #[test]
    fn later_serialized_start_does_not_remove_newer_run() {
        let attribution = RunAttribution::new();
        attribution.start_run("newer", 5, HashSet::new());
        attribution.start_run("older", 2, HashSet::new());

        let snapshot =
            attribution.observe("newer", 6, HashSet::new(), false, None, |_, _, _, _| {
                HashSet::new()
            });

        assert_eq!(snapshot.start_generation, 5);
    }

    #[test]
    fn eviction_removes_only_notified_run_state() {
        let attribution = RunAttribution::new();
        attribution.start_run("old", 1, HashSet::new());
        attribution.start_run("current", 2, HashSet::new());
        attribution.record_completion("pending", Utc::now());

        attribution.evict_runs(&["old".to_owned(), "pending".to_owned()]);

        let state = attribution.state.lock();
        assert!(!state.baselines.contains_key("old"));
        assert!(state.baselines.contains_key("current"));
        assert!(!state.pending_completions.contains_key("pending"));
    }

    #[test]
    fn concurrent_terminal_observers_share_one_frozen_result() {
        let attribution = Arc::new(RunAttribution::new());
        attribution.start_run("run", 1, HashSet::new());
        attribution.record_completion("run", Utc::now());
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for path in ["one.rs", "two.rs"] {
            let attribution = Arc::clone(&attribution);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                attribution.observe("run", 3, HashSet::new(), true, None, |_, _, _, _| {
                    HashSet::from([path.to_owned()])
                })
            }));
        }
        barrier.wait();
        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();

        assert_eq!(first, second);
        assert_eq!(first.attribution_generation, 3);
        assert_eq!(first.changed.count, 1);
    }
}

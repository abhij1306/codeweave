//! A Bash run's record and the one state machine that moves it from queued to
//! terminal.
//!
//! ```text
//! queued ──► running ──► succeeded | failed | timed_out | cancelled
//!   │           │
//!   │           ▼
//!   └──────► cancelling ──► cancelled | failed
//!   └──► failed | cancelled            (before a process starts)
//! ```
//!
//! [`RunRecord::transition`] is the only code that changes the status, and it
//! sets the terminal fields (end time, released process group) exactly once.
use super::output::RunOutput;
use crate::process_runtime::ProcessGroup;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::Cancelling,
            Self::Succeeded,
            Self::Failed,
            Self::TimedOut,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|status| status.as_str() == value)
    }

    /// A terminal run has ended and never changes status again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Cancelled
        )
    }

    /// Whether a clamped output view keeps the end (where errors and the latest
    /// progress are) rather than the beginning. Only a success keeps the head.
    pub(crate) fn keeps_output_tail(self) -> bool {
        self != Self::Succeeded
    }

    fn can_become(self, next: Self) -> bool {
        use RunStatus::*;
        matches!(
            (self, next),
            (Queued, Running | Cancelling | Failed | Cancelled)
                | (
                    Running,
                    Cancelling | Succeeded | Failed | TimedOut | Cancelled
                )
                | (Cancelling, Cancelled | Failed)
        )
    }
}

/// How the process ended, as the execution loop observed it.
#[derive(Debug)]
pub(crate) enum ProcessOutcome {
    Exited { code: Option<i32>, success: bool },
    WaitFailed(String),
    TimedOut { timeout_ms: u64 },
}

/// The workspace stamp a run's change attribution starts from.
#[derive(Debug, Clone)]
pub(crate) struct ChangeBaseline {
    pub(crate) generation: u64,
    pub(crate) dirty: Arc<HashSet<String>>,
}

/// The changed paths attributed to a terminal run, fixed at the first terminal
/// observation.
#[derive(Debug, Clone)]
pub(crate) struct FrozenChanges {
    pub(crate) generation: u64,
    pub(crate) paths: HashSet<String>,
}

/// What the workspace needs to attribute changes to a run.
#[derive(Debug, Clone)]
pub(crate) struct RunChangeWindow {
    pub(crate) baseline: ChangeBaseline,
    pub(crate) ended_at: Option<DateTime<Utc>>,
    pub(crate) frozen: Option<FrozenChanges>,
}

/// Result of a cancel request.
pub(crate) enum CancelRequest {
    /// The run already ended; nothing changed.
    AlreadyTerminal,
    /// The run is now cancelling; stop this group if a process is running.
    Requested(Option<ProcessGroup>),
}

#[derive(Debug)]
pub(crate) struct RunRecord {
    run_id: String,
    sequence: u64,
    command: String,
    cwd: PathBuf,
    started_at: DateTime<Utc>,
    status: RunStatus,
    ended_at: Option<DateTime<Utc>>,
    exit_code: Option<i32>,
    cancel_requested: bool,
    process: Option<ProcessGroup>,
    output: RunOutput,
    /// Supervisor messages shown after the output (warnings, why it ended).
    notes: Vec<String>,
    baseline: Option<ChangeBaseline>,
    frozen_changes: Option<FrozenChanges>,
}

impl RunRecord {
    pub(crate) fn queued(
        run_id: String,
        sequence: u64,
        command: String,
        cwd: PathBuf,
        output_limit: usize,
    ) -> Self {
        Self {
            run_id,
            sequence,
            command,
            cwd,
            started_at: Utc::now(),
            status: RunStatus::Queued,
            ended_at: None,
            exit_code: None,
            cancel_requested: false,
            process: None,
            output: RunOutput::new(output_limit),
            notes: Vec::new(),
            baseline: None,
            frozen_changes: None,
        }
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }
    pub(crate) fn command(&self) -> &str {
        &self.command
    }
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
    pub(crate) fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
    pub(crate) fn status(&self) -> RunStatus {
        self.status
    }
    pub(crate) fn ended_at(&self) -> Option<DateTime<Utc>> {
        self.ended_at
    }
    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.process.as_ref().and_then(ProcessGroup::id)
    }
    pub(crate) fn output(&self) -> &RunOutput {
        &self.output
    }
    pub(crate) fn output_mut(&mut self) -> &mut RunOutput {
        &mut self.output
    }
    pub(crate) fn notes(&self) -> &[String] {
        &self.notes
    }

    pub(crate) fn add_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// The single status writer. Rejects illegal moves (anything out of a
    /// terminal status included) and, on entering a terminal status, records the
    /// end time and releases the process group.
    fn transition(&mut self, next: RunStatus) -> bool {
        if !self.status.can_become(next) {
            return false;
        }
        self.status = next;
        if next.is_terminal() {
            self.ended_at = Some(Utc::now());
            self.process = None;
        }
        true
    }

    /// End the run. Returns false, changing nothing, if it already ended.
    pub(crate) fn finish(
        &mut self,
        status: RunStatus,
        exit_code: Option<i32>,
        note: Option<String>,
    ) -> bool {
        debug_assert!(status.is_terminal());
        if !self.transition(status) {
            return false;
        }
        self.exit_code = exit_code;
        self.notes.extend(note);
        true
    }

    /// End the run from what its process did. A requested cancel turns any
    /// exit or timeout into `cancelled`.
    pub(crate) fn finish_process(&mut self, outcome: ProcessOutcome) -> bool {
        let cancelled = self.cancel_requested;
        let (status, exit_code, note) = match outcome {
            ProcessOutcome::Exited { code, .. } if cancelled => {
                (RunStatus::Cancelled, code, Some(CANCELLED_NOTE.to_owned()))
            }
            ProcessOutcome::Exited { code, success } => (
                if success {
                    RunStatus::Succeeded
                } else {
                    RunStatus::Failed
                },
                code,
                None,
            ),
            ProcessOutcome::WaitFailed(error) => (
                RunStatus::Failed,
                None,
                Some(format!("Bash wait failed: {error}")),
            ),
            ProcessOutcome::TimedOut { .. } if cancelled => {
                (RunStatus::Cancelled, None, Some(CANCELLED_NOTE.to_owned()))
            }
            ProcessOutcome::TimedOut { timeout_ms } => (
                RunStatus::TimedOut,
                None,
                Some(format!(
                    "Bash run exceeded timeout of {timeout_ms} ms; partial output was retained."
                )),
            ),
        };
        self.finish(status, exit_code, note)
    }

    /// End a run whose cancel arrived before its process started. Returns false
    /// if no cancel was requested.
    pub(crate) fn finish_if_cancelled_before_start(&mut self) -> bool {
        self.cancel_requested
            && self.finish(
                RunStatus::Cancelled,
                None,
                Some("Bash run was cancelled before the process started.".to_owned()),
            )
    }

    /// Record the started process. Returns true if a cancel was already
    /// requested, in which case the caller must stop the group.
    pub(crate) fn start_process(&mut self, group: ProcessGroup) -> bool {
        self.started_at = Utc::now();
        if !self.cancel_requested {
            self.transition(RunStatus::Running);
        }
        if !self.status.is_terminal() {
            self.process = Some(group);
        }
        self.cancel_requested
    }

    pub(crate) fn request_cancel(&mut self) -> CancelRequest {
        if !self.transition(RunStatus::Cancelling) {
            return if self.status == RunStatus::Cancelling {
                CancelRequest::Requested(self.process.clone())
            } else {
                CancelRequest::AlreadyTerminal
            };
        }
        self.cancel_requested = true;
        CancelRequest::Requested(self.process.clone())
    }

    pub(crate) fn set_baseline(&mut self, baseline: ChangeBaseline) {
        self.baseline = Some(baseline);
    }

    /// The attribution window, installing `fallback` as the baseline if the run
    /// has none yet.
    pub(crate) fn change_window(&mut self, fallback: ChangeBaseline) -> RunChangeWindow {
        let baseline = self.baseline.get_or_insert(fallback).clone();
        RunChangeWindow {
            baseline,
            ended_at: self.ended_at,
            frozen: self.frozen_changes.clone(),
        }
    }

    /// Freeze the terminal attribution; the first freeze wins.
    pub(crate) fn freeze_changes(&mut self, changes: FrozenChanges) -> FrozenChanges {
        self.frozen_changes.get_or_insert(changes).clone()
    }
}

const CANCELLED_NOTE: &str = "Bash run was cancelled; partial output was retained.";

#[cfg(test)]
mod tests {
    use super::RunStatus;

    #[test]
    fn statuses_use_their_wire_names() {
        let expected = [
            (RunStatus::Queued, "queued", false),
            (RunStatus::Running, "running", false),
            (RunStatus::Cancelling, "cancelling", false),
            (RunStatus::Succeeded, "succeeded", true),
            (RunStatus::Failed, "failed", true),
            (RunStatus::TimedOut, "timed_out", true),
            (RunStatus::Cancelled, "cancelled", true),
        ];
        for (status, name, terminal) in expected {
            assert_eq!(serde_json::to_value(status).unwrap(), name);
            assert_eq!(RunStatus::parse(name), Some(status));
            assert_eq!(status.is_terminal(), terminal, "{name}");
        }
        assert_eq!(RunStatus::parse("done"), None);
    }
}

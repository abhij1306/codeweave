//! The reconciled state of one workspace: the code index, the cached `git status`
//! result with its stale flag, and the [`Stamp`] (generation + snapshot id) that
//! names that combination. It also owns the mutation gate and the change ledger.
//!
//! # Lock order
//!
//! There are three levels, always taken in this order:
//!
//! 1. **Mutation gate** ([`ReconciledState::exclusive`]): an async, owned
//!    [`MutationPermit`]. Held by `code_edit` (through post-apply validation) and by
//!    the mutating Git actions `stage`, `commit` and `restore`, so they never overlap.
//! 2. **Writer** ([`ReconciledState::writer`]): a blocking mutex that serializes every
//!    write to the reconciled state (reconcile, forced refresh, edit commit, the
//!    repository-status setters). [`StateWriter`] is the only route to write access.
//!    Holding it does not block readers.
//! 3. **State lock**: one `RwLock` over the reconciled fields. Readers get a
//!    [`StateView`], which reads every field (and the stamp) from one guard. A writer
//!    applies each change in one [`StateWriter::update`] call under the write guard,
//!    so readers never see a partially applied change.
//!
//! The permit is only obtainable from async code, and both [`StateWriter`] and
//! [`StateView`] wrap `parking_lot` guards, which are `!Send`: neither can be held
//! across an `.await` in a spawned task, so neither can be held while waiting for
//! the permit. A writer reads through [`StateWriter::read`], never through a second
//! [`ReconciledState::read`] call, and code holding a [`StateView`] must drop it
//! before asking for a writer. An [`StateWriter::update`] closure must not call back
//! into [`ReconciledState::read`]. The [`ChangeLedger`] locks are leaves: they are
//! taken briefly, never while waiting for any other lock in this module.

mod ledger;

pub(crate) use ledger::{temp_file_name, ChangeLedger, RunWindow};

use crate::error::{AppError, AppResult};
use crate::index::{CodeIndex, WorkspaceExclusions};
use crate::repository::RepoStatus;
use parking_lot::{Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::path::PathBuf;
use std::sync::Arc;

/// The identity of one reconciled state: an instance-local generation counter and
/// the content-derived snapshot id. Always read together from one [`StateView`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Stamp {
    pub generation: u64,
    pub snapshot_id: String,
}

struct Reconciled {
    index: CodeIndex,
    repo_status: RepoStatus,
    repo_status_stale: bool,
    generation: u64,
    snapshot_id: String,
}

impl Reconciled {
    fn recompute_snapshot(&mut self) {
        self.snapshot_id = self.index.snapshot_id(&self.repo_status.head);
    }
}

/// Owned permit from the mutation gate. Dropping it lets the next mutation run.
pub(crate) struct MutationPermit {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Whether a repository-status refresh changed anything callers care about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RepoStatusChange {
    pub(crate) changed: bool,
    pub(crate) head_changed: bool,
}

pub(crate) struct ReconciledState {
    workspace_id: String,
    gate: Arc<tokio::sync::Mutex<()>>,
    writer: Mutex<()>,
    shared: Arc<RwLock<Reconciled>>,
    ledger: ChangeLedger,
}

impl ReconciledState {
    /// Build the initial state (generation 1). A failed initial `git status` starts
    /// from an empty status marked stale, like any later failed refresh.
    pub(crate) fn new(
        workspace_id: &str,
        root: PathBuf,
        exclusions: WorkspaceExclusions,
        index: CodeIndex,
        repo_status: AppResult<RepoStatus>,
    ) -> Self {
        let (repo_status, repo_status_stale) = match repo_status {
            Ok(status) => (status, false),
            Err(error) => {
                warn_stale(workspace_id, &error);
                (RepoStatus::default(), true)
            }
        };
        let mut reconciled = Reconciled {
            index,
            repo_status,
            repo_status_stale,
            generation: 1,
            snapshot_id: String::new(),
        };
        reconciled.recompute_snapshot();
        Self {
            workspace_id: workspace_id.to_owned(),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            writer: Mutex::new(()),
            shared: Arc::new(RwLock::new(reconciled)),
            ledger: ChangeLedger::new(root, exclusions),
        }
    }

    /// Wait for the mutation gate. Hold the permit for the whole mutation,
    /// including any work that reports on the mutated files.
    pub(crate) async fn exclusive(&self) -> MutationPermit {
        MutationPermit {
            _guard: Arc::clone(&self.gate).lock_owned().await,
        }
    }

    /// Serialize with every other writer and obtain write access.
    pub(crate) fn writer(&self) -> StateWriter<'_> {
        StateWriter {
            _serial: self.writer.lock(),
            shared: &self.shared,
            workspace_id: &self.workspace_id,
        }
    }

    /// A consistent read of the reconciled fields.
    pub(crate) fn read(&self) -> StateView<'_> {
        StateView {
            guard: self.shared.read(),
        }
    }

    /// A cloneable read-only handle for services outside the workspace.
    pub(crate) fn reader(&self) -> StateReader {
        StateReader {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn ledger(&self) -> &ChangeLedger {
        &self.ledger
    }
}

/// Exclusive write access to the reconciled state. Holding it excludes every other
/// writer; readers keep seeing the last published state until an
/// [`update`](Self::update) completes.
pub(crate) struct StateWriter<'a> {
    _serial: MutexGuard<'a, ()>,
    shared: &'a RwLock<Reconciled>,
    workspace_id: &'a str,
}

impl StateWriter<'_> {
    pub(crate) fn read(&self) -> StateView<'_> {
        StateView {
            guard: self.shared.read(),
        }
    }

    /// Apply one change atomically: readers see all of it or none of it.
    pub(crate) fn update<R>(&mut self, change: impl FnOnce(&mut StateUpdate<'_>) -> R) -> R {
        let mut guard = self.shared.write();
        change(&mut StateUpdate {
            state: &mut guard,
            workspace_id: self.workspace_id,
        })
    }
}

/// The reconciled fields while one [`StateWriter::update`] is applying a change.
pub(crate) struct StateUpdate<'a> {
    state: &'a mut Reconciled,
    workspace_id: &'a str,
}

impl StateUpdate<'_> {
    pub(crate) fn index(&self) -> &CodeIndex {
        &self.state.index
    }

    pub(crate) fn index_mut(&mut self) -> &mut CodeIndex {
        &mut self.state.index
    }

    /// Store a `git status` result. On success the status is stored and the stale
    /// flag cleared. On failure the previous status is kept, the stale flag is set,
    /// a warning is logged, and the error is handed back for callers that report it.
    pub(crate) fn apply_repo_status(
        &mut self,
        result: AppResult<RepoStatus>,
    ) -> AppResult<RepoStatusChange> {
        match result {
            Ok(status) => {
                let change = RepoStatusChange {
                    changed: status != self.state.repo_status,
                    head_changed: status.head != self.state.repo_status.head,
                };
                self.state.repo_status = status;
                self.state.repo_status_stale = false;
                Ok(change)
            }
            Err(error) => {
                warn_stale(self.workspace_id, &error);
                self.state.repo_status_stale = true;
                Err(error)
            }
        }
    }

    /// Advance the generation and return the new value.
    pub(crate) fn bump_generation(&mut self) -> u64 {
        self.state.generation += 1;
        self.state.generation
    }

    /// Recompute the snapshot id from the current index and HEAD.
    pub(crate) fn recompute_snapshot(&mut self) {
        self.state.recompute_snapshot();
    }
}

fn warn_stale(workspace_id: &str, error: &AppError) {
    tracing::warn!(
        workspace = %workspace_id,
        error = %error,
        "git status refresh failed; repo_status may be stale"
    );
}

/// One consistent read of the reconciled state.
pub struct StateView<'a> {
    guard: RwLockReadGuard<'a, Reconciled>,
}

impl StateView<'_> {
    pub fn index(&self) -> &CodeIndex {
        &self.guard.index
    }

    pub fn repo_status(&self) -> &RepoStatus {
        &self.guard.repo_status
    }

    /// True while the last `git status` refresh failed and `repo_status` may be
    /// out of date (D8). Cleared by the next successful refresh.
    pub fn repo_status_stale(&self) -> bool {
        self.guard.repo_status_stale
    }

    pub fn stamp(&self) -> Stamp {
        Stamp {
            generation: self.guard.generation,
            snapshot_id: self.guard.snapshot_id.clone(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.guard.generation
    }

    pub fn snapshot_id(&self) -> &str {
        &self.guard.snapshot_id
    }
}

/// Cloneable read-only handle into a workspace's reconciled state.
#[derive(Clone)]
pub struct StateReader {
    shared: Arc<RwLock<Reconciled>>,
}

impl StateReader {
    /// A handle over a standalone index that no workspace writes to (for services
    /// used without a workspace, and their tests).
    pub fn detached(index: CodeIndex, snapshot_id: impl Into<String>) -> Self {
        Self {
            shared: Arc::new(RwLock::new(Reconciled {
                index,
                repo_status: RepoStatus::default(),
                repo_status_stale: false,
                generation: 1,
                snapshot_id: snapshot_id.into(),
            })),
        }
    }

    pub fn read(&self) -> StateView<'_> {
        StateView {
            guard: self.shared.read(),
        }
    }
}

//! Operating-system process trees: spawning a child into its own group and
//! terminating everything it started.
//!
//! [`ProcessGroup`] is the async-friendly seam used by Bash runs: its calls never
//! block, escalation from a polite stop to a forced kill is a tracked task, and
//! dropping the last handle kills the group. [`ProcessTree`] serves the
//! synchronous callers (Git, language servers) that run on their own threads.
use parking_lot::Mutex;
use std::fmt;
use std::io;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// The platform operations a process group needs. Dropping the adapter must
/// release the group; the Unix and Windows adapters kill what is left.
pub(crate) trait GroupAdapter: Send + Sync + 'static {
    /// Process id of the group leader, reported to clients.
    fn id(&self) -> Option<u32>;
    /// Ask the group to stop. May be immediate on platforms without a polite stop.
    fn terminate(&self);
    /// Force the group to stop.
    fn kill(&self);
}

/// One opaque handle to a spawned process tree. Clones share the group; when
/// the last clone drops, any escalation task is cancelled and the group is killed.
#[derive(Clone)]
pub struct ProcessGroup {
    inner: Arc<GroupInner>,
}

struct GroupInner {
    adapter: Box<dyn GroupAdapter>,
    escalation: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for GroupInner {
    fn drop(&mut self) {
        if let Some(task) = self.escalation.get_mut().take() {
            task.abort();
        }
        // The adapter's own Drop kills whatever is left of the group.
    }
}

impl fmt::Debug for ProcessGroup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessGroup")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

/// A child spawned into its own process group.
pub struct SpawnedGroup {
    pub child: tokio::process::Child,
    pub group: ProcessGroup,
    /// Set when the group could not be fully established; cancellation then
    /// falls back to a best-effort mechanism.
    pub warning: Option<String>,
}

impl ProcessGroup {
    pub(crate) fn from_adapter(adapter: impl GroupAdapter) -> Self {
        Self {
            inner: Arc::new(GroupInner {
                adapter: Box::new(adapter),
                escalation: Mutex::new(None),
            }),
        }
    }

    /// Spawn `command` as the leader of a new process group (Unix) or inside a
    /// kill-on-close Job object (Windows).
    pub fn spawn(command: &mut tokio::process::Command) -> io::Result<SpawnedGroup> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let child = command.spawn()?;
        let pid = child.id();
        let (group, warning) = Self::attach(pid);
        Ok(SpawnedGroup {
            child,
            group,
            warning,
        })
    }

    #[cfg(unix)]
    fn attach(pid: Option<u32>) -> (Self, Option<String>) {
        match pid.and_then(unix::UnixGroup::new) {
            Some(group) => (Self::from_adapter(group), None),
            None => (
                Self::from_adapter(NoGroup),
                Some("CodeWeave warning: spawned task has no process group".to_owned()),
            ),
        }
    }

    #[cfg(windows)]
    fn attach(pid: Option<u32>) -> (Self, Option<String>) {
        let Some(pid) = pid else {
            return (
                Self::from_adapter(NoGroup),
                Some("CodeWeave warning: spawned task has no process id".to_owned()),
            );
        };
        match windows::WindowsJob::assign(pid) {
            Ok(job) => (
                Self::from_adapter(windows::WindowsGroup::new(pid, Some(job))),
                None,
            ),
            Err(error) => (
                Self::from_adapter(windows::WindowsGroup::new(pid, None)),
                Some(format!(
                    "CodeWeave warning: Windows Job Object assignment failed; falling back to taskkill for cancellation: {error}"
                )),
            ),
        }
    }

    pub fn id(&self) -> Option<u32> {
        self.inner.adapter.id()
    }

    /// Ask the group to stop and, unless it is dropped first, force-kill it
    /// after `grace`. Never blocks; the escalation runs as a tracked task that
    /// the last handle's Drop cancels. Repeated calls re-send the stop request
    /// but schedule only one escalation.
    pub fn terminate(&self, grace: Duration) {
        self.inner.adapter.terminate();
        let mut escalation = self.inner.escalation.lock();
        if escalation.is_some() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let group: Weak<GroupInner> = Arc::downgrade(&self.inner);
                *escalation = Some(runtime.spawn(async move {
                    tokio::time::sleep(grace).await;
                    if let Some(inner) = group.upgrade() {
                        inner.adapter.kill();
                    }
                }));
            }
            Err(_) => self.inner.adapter.kill(),
        }
    }
}

/// Stands in when the platform group could not be created.
struct NoGroup;

impl GroupAdapter for NoGroup {
    fn id(&self) -> Option<u32> {
        None
    }
    fn terminate(&self) {}
    fn kill(&self) {}
}

#[cfg(unix)]
mod unix {
    use super::GroupAdapter;

    /// A process group led by the spawned child (`process_group(0)`), so its id
    /// is the child's pid.
    pub(super) struct UnixGroup {
        pgid: libc::pid_t,
    }

    impl UnixGroup {
        pub(super) fn new(pid: u32) -> Option<Self> {
            // Never signal group 0 (our own) or 1 (init).
            libc::pid_t::try_from(pid)
                .ok()
                .filter(|pgid| *pgid > 1)
                .map(|pgid| Self { pgid })
        }

        pub(super) fn signal(&self, signal: libc::c_int) {
            // SAFETY: killpg only sends a signal; an exited group yields ESRCH.
            unsafe {
                libc::killpg(self.pgid, signal);
            }
        }

        /// Signal without taking ownership, so nothing is killed on drop.
        pub(super) fn signal_pid(pid: u32, signal: libc::c_int) {
            if let Some(group) = Self::new(pid) {
                group.signal(signal);
                std::mem::forget(group);
            }
        }
    }

    impl GroupAdapter for UnixGroup {
        fn id(&self) -> Option<u32> {
            u32::try_from(self.pgid).ok()
        }
        fn terminate(&self) {
            self.signal(libc::SIGTERM);
        }
        fn kill(&self) {
            self.signal(libc::SIGKILL);
        }
    }

    impl Drop for UnixGroup {
        fn drop(&mut self) {
            self.signal(libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::GroupAdapter;
    use std::io;
    use std::process::Stdio;

    /// A Job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing the handle
    /// kills every process still assigned to it.
    #[derive(Debug)]
    pub(crate) struct WindowsJob {
        handle: usize,
    }

    impl WindowsJob {
        fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
            self.handle as _
        }

        pub(crate) fn assign(pid: u32) -> io::Result<Self> {
            use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
                PROCESS_TERMINATE,
            };
            // SAFETY: every handle is checked before use; the job handle is owned
            // by `holder` from creation, so each error path closes it on drop.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job == 0 as _ {
                    return Err(io::Error::last_os_error());
                }
                let holder = Self {
                    handle: job as usize,
                };
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == FALSE
                {
                    return Err(io::Error::last_os_error());
                }
                let process = OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                    FALSE,
                    pid,
                );
                if process == 0 as _ {
                    return Err(io::Error::last_os_error());
                }
                let assigned = AssignProcessToJobObject(job, process);
                let assign_error = (assigned == FALSE).then(io::Error::last_os_error);
                CloseHandle(process);
                if let Some(error) = assign_error {
                    return Err(error);
                }
                Ok(holder)
            }
        }

        pub(crate) fn terminate(&self) -> io::Result<()> {
            use windows_sys::Win32::Foundation::FALSE;
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: the handle is a live job handle owned by `self`.
            if unsafe { TerminateJobObject(self.raw(), 1) } == FALSE {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for WindowsJob {
        fn drop(&mut self) {
            // SAFETY: the handle is owned by `self` and closed exactly once.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.raw());
            }
        }
    }

    /// Start `taskkill /T /F` for `pid` without waiting for it.
    pub(super) fn spawn_taskkill(pid: u32) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    /// Owns the run's Job object. Without one (assignment failed), stopping falls
    /// back to `taskkill`; dropping then does nothing, because the pid may
    /// already belong to another process.
    pub(super) struct WindowsGroup {
        pid: u32,
        job: Option<WindowsJob>,
    }

    impl WindowsGroup {
        pub(super) fn new(pid: u32, job: Option<WindowsJob>) -> Self {
            Self { pid, job }
        }
    }

    impl GroupAdapter for WindowsGroup {
        fn id(&self) -> Option<u32> {
            Some(self.pid)
        }
        fn terminate(&self) {
            self.kill();
        }
        fn kill(&self) {
            if self.job.as_ref().is_some_and(|job| job.terminate().is_ok()) {
                return;
            }
            spawn_taskkill(self.pid);
        }
    }
}

/// Process-tree handle for synchronous callers that wait on their own thread.
#[derive(Debug)]
pub struct ProcessTree {
    pid: u32,
    #[cfg(windows)]
    job: Option<windows::WindowsJob>,
}

impl ProcessTree {
    /// Track the tree rooted at `pid`. On Unix the child must have been spawned
    /// with `process_group(0)`; on Windows it is assigned to a Job object.
    pub fn attach(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(windows)]
            job: windows::WindowsJob::assign(pid).ok(),
        }
    }

    /// Stop the tree. On Unix the group gets SIGTERM, then SIGKILL two seconds
    /// later from a helper thread; on Windows the Job object (or `taskkill`)
    /// stops it at once.
    pub fn terminate(&self) {
        #[cfg(windows)]
        {
            if self.job.as_ref().is_some_and(|job| job.terminate().is_ok()) {
                return;
            }
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .status();
        }
        #[cfg(unix)]
        {
            unix::UnixGroup::signal_pid(self.pid, libc::SIGTERM);
            let pid = self.pid;
            // The caller blocks on `wait()`, so the forced stop must come from
            // another thread.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(2));
                unix::UnixGroup::signal_pid(pid, libc::SIGKILL);
            });
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted process group for deterministic supervisor tests.
    use super::{GroupAdapter, ProcessGroup};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    /// How a fake process reacts to stop requests.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FakeReaction {
        /// Exits with this code as soon as it is launched.
        ExitsWith(i32),
        /// Runs until asked to terminate.
        StopsOnTerminate,
        /// Ignores terminate; only a forced kill stops it.
        IgnoresTerminate,
    }

    /// What happened to fake process groups, in order.
    pub(crate) type FakeEvents = Arc<Mutex<Vec<&'static str>>>;

    struct FakeAdapter {
        reaction: FakeReaction,
        events: FakeEvents,
        exit: Mutex<Option<oneshot::Sender<Option<i32>>>>,
    }

    impl FakeAdapter {
        fn exit(&self, code: Option<i32>) {
            if let Some(sender) = self.exit.lock().take() {
                let _ = sender.send(code);
            }
        }
    }

    impl GroupAdapter for FakeAdapter {
        fn id(&self) -> Option<u32> {
            Some(4242)
        }
        fn terminate(&self) {
            self.events.lock().push("terminate");
            if self.reaction == FakeReaction::StopsOnTerminate {
                self.exit(None);
            }
        }
        fn kill(&self) {
            self.events.lock().push("kill");
            self.exit(None);
        }
    }

    impl Drop for FakeAdapter {
        fn drop(&mut self) {
            self.events.lock().push("dropped");
        }
    }

    /// A fake group and the receiver that resolves when its process exits.
    pub(crate) fn fake_group(
        reaction: FakeReaction,
        events: FakeEvents,
    ) -> (ProcessGroup, oneshot::Receiver<Option<i32>>) {
        let (sender, receiver) = oneshot::channel();
        let adapter = FakeAdapter {
            reaction,
            events,
            exit: Mutex::new(Some(sender)),
        };
        if let FakeReaction::ExitsWith(code) = reaction {
            adapter.exit(Some(code));
        }
        (ProcessGroup::from_adapter(adapter), receiver)
    }
}

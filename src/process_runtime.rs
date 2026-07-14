use std::io;
#[cfg(windows)]
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Combined,
    Stdout,
    Stderr,
}

impl OutputStream {
    pub fn parse(value: Option<&str>) -> io::Result<Self> {
        match value.unwrap_or("combined") {
            "combined" => Ok(Self::Combined),
            "stdout" => Ok(Self::Stdout),
            "stderr" => Ok(Self::Stderr),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown Bash output stream: {other}"),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Combined => "combined",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

pub fn strip_ansi(value: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }
    let mut state = State::Text;
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        state = match state {
            State::Text if ch == '\u{1b}' => State::Escape,
            State::Text => {
                output.push(ch);
                State::Text
            }
            State::Escape if ch == '[' => State::Csi,
            State::Escape if ch == ']' => State::Osc,
            State::Escape => State::Text,
            State::Csi if ('@'..='~').contains(&ch) => State::Text,
            State::Csi => State::Csi,
            State::Osc if ch == '\u{7}' => State::Text,
            State::Osc if ch == '\u{1b}' => State::OscEscape,
            State::Osc => State::Osc,
            State::OscEscape if ch == '\\' => State::Text,
            State::OscEscape if ch == '\u{1b}' => State::OscEscape,
            State::OscEscape => State::Osc,
        };
    }
    output
}

#[derive(Debug)]
pub struct WindowsJob {
    #[cfg(windows)]
    handle: usize,
}

#[cfg(windows)]
impl WindowsJob {
    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle as _
    }

    pub fn assign(pid: u32) -> io::Result<Arc<Self>> {
        use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 as _ {
                return Err(io::Error::last_os_error());
            }
            let holder = Arc::new(Self {
                handle: job as usize,
            });
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

    pub fn terminate(&self) -> io::Result<()> {
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if unsafe { TerminateJobObject(self.raw(), 1) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.raw());
        }
    }
}

pub fn terminate_process_tree(pid: u32, job: Option<&WindowsJob>) {
    #[cfg(windows)]
    {
        if let Some(job) = job {
            if job.terminate().is_ok() {
                return;
            }
        }
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = job;
        let group = format!("-{pid}");
        let _ = std::process::Command::new("kill")
            .args(["-TERM", "--", &group])
            .status();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let group = format!("-{pid}");
            let alive = std::process::Command::new("kill")
                .args(["-0", "--", &group])
                .status()
                .is_ok_and(|status| status.success());
            if alive {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", "--", &group])
                    .status();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_csi_and_osc_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
    }
}

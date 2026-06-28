use crate::bash::RunRecord;
use crate::model::OutputFilter;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const LOG_HEAD_BYTES: usize = 12 * 1024 * 1024;
const LOG_TAIL_BYTES: usize = 4 * 1024 * 1024;
const LIVE_TAIL_MAX_BYTES: usize = 512 * 1024;
const SHELL_READ_CHUNK_BYTES: u64 = 64 * 1024;
const OMITTED_MARKER: &[u8] = b"\n... Bash log middle omitted; tail follows ...\n";

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

#[derive(Debug, Clone)]
pub struct RunLogPaths {
    pub combined: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

impl RunLogPaths {
    pub fn new(log_root: &Path, run_id: &str) -> Self {
        Self {
            combined: log_root.join(format!("{run_id}.log")),
            stdout: log_root.join(format!("{run_id}.stdout.log")),
            stderr: log_root.join(format!("{run_id}.stderr.log")),
        }
    }

    pub fn path(&self, stream: OutputStream) -> &Path {
        match stream {
            OutputStream::Combined => &self.combined,
            OutputStream::Stdout => &self.stdout,
            OutputStream::Stderr => &self.stderr,
        }
    }

    pub fn all(&self) -> [&Path; 3] {
        [&self.combined, &self.stdout, &self.stderr]
    }
}

struct StreamSink {
    file: File,
    head_written: usize,
    overflowed: bool,
    tail: Vec<u8>,
}

impl StreamSink {
    async fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::create(path).await?,
            head_written: 0,
            overflowed: false,
            tail: Vec::new(),
        })
    }

    async fn push(&mut self, chunk: &[u8]) -> io::Result<()> {
        let remaining = LOG_HEAD_BYTES.saturating_sub(self.head_written);
        let retained = remaining.min(chunk.len());
        if retained > 0 {
            self.file.write_all(&chunk[..retained]).await?;
            self.file.flush().await?;
            self.head_written += retained;
        }
        if retained < chunk.len() {
            self.overflowed = true;
            append_rolling(&mut self.tail, &chunk[retained..], LOG_TAIL_BYTES);
        }
        Ok(())
    }

    async fn finish(mut self) -> io::Result<bool> {
        if self.overflowed {
            self.file.write_all(OMITTED_MARKER).await?;
            self.file.write_all(&self.tail).await?;
        }
        self.file.flush().await?;
        Ok(self.overflowed)
    }
}

fn append_rolling(target: &mut Vec<u8>, chunk: &[u8], limit: usize) {
    if limit == 0 {
        target.clear();
        return;
    }
    if chunk.len() >= limit {
        target.clear();
        target.extend_from_slice(&chunk[chunk.len() - limit..]);
        return;
    }
    let overflow = target
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(limit);
    if overflow > 0 {
        target.drain(..overflow);
    }
    target.extend_from_slice(chunk);
}

enum ShellLine {
    Data(Vec<u8>),
    Closed,
}

#[derive(Debug)]
pub struct WarmShell {
    child: Child,
    stdin: ChildStdin,
    stdout_rx: UnboundedReceiver<ShellLine>,
    stderr_rx: UnboundedReceiver<ShellLine>,
    readers: Vec<tokio::task::JoinHandle<()>>,
    pid: Option<u32>,
    #[cfg(windows)]
    job: Option<Arc<WindowsJob>>,
}

pub struct WarmOutcome {
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub limited: bool,
    pub needs_respawn: bool,
}

impl WarmShell {
    pub fn spawn(executable: &str) -> io::Result<Self> {
        let mut cmd = Command::new(executable);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let pid = child.id();

        #[cfg(windows)]
        let job = pid.and_then(|p| WindowsJob::assign(p).ok());

        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();

        let stdout_reader = tokio::spawn(pump_lines(stdout, stdout_tx));
        let stderr_reader = tokio::spawn(pump_lines(stderr, stderr_tx));

        Ok(Self {
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            readers: vec![stdout_reader, stderr_reader],
            pid,
            #[cfg(windows)]
            job,
        })
    }

    pub async fn run(
        &mut self,
        command: &str,
        cwd: &Path,
        timeout_ms: u64,
        paths: &RunLogPaths,
    ) -> io::Result<WarmOutcome> {
        while let Ok(ShellLine::Data(_)) = self.stdout_rx.try_recv() {}
        while let Ok(ShellLine::Data(_)) = self.stderr_rx.try_recv() {}

        let marker = uuid::Uuid::new_v4().simple().to_string();
        let cwd_shell = to_shell_path(cwd);
        let command_quoted = quote_single(command);

        let script = format!(
            "(\n__cw_cwd={}\nif [ -n \"${{MSYSTEM:-}}\" ] && command -v cygpath >/dev/null 2>&1; then\n  __cw_cwd=$(cygpath -u \"$__cw_cwd\") || exit $?\nelif command -v wslpath >/dev/null 2>&1; then\n  __cw_cwd=$(wslpath -u \"$__cw_cwd\") || exit $?\nelif command -v cygpath >/dev/null 2>&1; then\n  __cw_cwd=$(cygpath -u \"$__cw_cwd\") || exit $?\nfi\ncd \"$__cw_cwd\" && eval {}\n) </dev/null\n__cw_ec=$?\nprintf '\\n%s %d\\n' '{}' \"$__cw_ec\"\nprintf '\\n%s\\n' '{}' >&2\n",
            quote_single(&cwd_shell),
            command_quoted,
            marker,
            marker
        );

        self.stdin.write_all(script.as_bytes()).await?;
        self.stdin.flush().await?;

        let mut combined_sink = StreamSink::create(&paths.combined).await?;
        let mut stdout_sink = StreamSink::create(&paths.stdout).await?;
        let mut stderr_sink = StreamSink::create(&paths.stderr).await?;

        let mut stdout_closed = false;
        let mut stderr_closed = false;
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut exit_code = None;
        let mut timed_out = false;

        let drain = async {
            let stdout_rx = &mut self.stdout_rx;
            let stderr_rx = &mut self.stderr_rx;

            loop {
                if (stdout_done || stdout_closed) && (stderr_done || stderr_closed) {
                    break;
                }

                tokio::select! {
                    line = stdout_rx.recv(), if !stdout_done && !stdout_closed => {
                        match line {
                            Some(ShellLine::Data(bytes)) => {
                                if let Some(code) = parse_marker(&bytes, &marker) {
                                    exit_code = code;
                                    stdout_done = true;
                                    continue;
                                }
                                combined_sink.push(&bytes).await?;
                                stdout_sink.push(&bytes).await?;
                            }
                            Some(ShellLine::Closed) | None => {
                                stdout_closed = true;
                            }
                        }
                    }
                    line = stderr_rx.recv(), if !stderr_done && !stderr_closed => {
                        match line {
                            Some(ShellLine::Data(bytes)) => {
                                if parse_marker(&bytes, &marker).is_some() {
                                    stderr_done = true;
                                    continue;
                                }
                                combined_sink.push(&bytes).await?;
                                stderr_sink.push(&bytes).await?;
                            }
                            Some(ShellLine::Closed) | None => {
                                stderr_closed = true;
                            }
                        }
                    }
                }
            }
            Ok::<_, io::Error>(())
        };

        match tokio::time::timeout(Duration::from_millis(timeout_ms), drain).await {
            Ok(result) => result?,
            Err(_) => {
                timed_out = true;
                if let Some(pid) = self.pid {
                    terminate_process_tree(
                        pid,
                        #[cfg(windows)]
                        self.job.as_deref(),
                        #[cfg(not(windows))]
                        None,
                    );
                }

                let grace = tokio::time::sleep(Duration::from_secs(5));
                tokio::pin!(grace);

                loop {
                    tokio::select! {
                        _ = &mut grace => break,
                        line = self.stdout_rx.recv() => {
                            if let Some(ShellLine::Data(bytes)) = line {
                                let _ = combined_sink.push(&bytes).await;
                                let _ = stdout_sink.push(&bytes).await;
                            } else {
                                stdout_closed = true;
                            }
                        }
                        line = self.stderr_rx.recv() => {
                            if let Some(ShellLine::Data(bytes)) = line {
                                let _ = combined_sink.push(&bytes).await;
                                let _ = stderr_sink.push(&bytes).await;
                            } else {
                                stderr_closed = true;
                            }
                        }
                    }
                    if stdout_closed && stderr_closed {
                        break;
                    }
                }
            }
        }

        let shell_died = stdout_closed || stderr_closed;
        if exit_code.is_none() && shell_died {
            exit_code = self.child.try_wait()?.and_then(|s| s.code());
            if exit_code.is_none() {
                exit_code = self.child.wait().await.ok().and_then(|s| s.code());
            }
        }

        let limited_combined = combined_sink.finish().await?;
        let limited_stdout = stdout_sink.finish().await?;
        let limited_stderr = stderr_sink.finish().await?;
        let limited = limited_combined || limited_stdout || limited_stderr;

        let status = if timed_out {
            "timed_out"
        } else if let Some(0) = exit_code {
            "succeeded"
        } else {
            "failed"
        };

        Ok(WarmOutcome {
            status,
            exit_code,
            limited,
            needs_respawn: timed_out || shell_died,
        })
    }
}

impl Drop for WarmShell {
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
    }
}

fn parse_marker(bytes: &[u8], marker: &str) -> Option<Option<i32>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix(marker) {
        let code_str = rest.trim();
        if code_str.is_empty() {
            return Some(None);
        }
        let code = code_str.parse::<i32>().ok()?;
        return Some(Some(code));
    }
    None
}

fn to_shell_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy();
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            return format!("//{}", unc.replace('\\', "/"));
        }
        path.strip_prefix(r"\\?\")
            .unwrap_or(&path)
            .replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.display().to_string()
    }
}

fn quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

async fn pump_lines<R: AsyncRead + Unpin>(reader: R, tx: UnboundedSender<ShellLine>) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut bytes = Vec::new();
        match (&mut reader)
            .take(SHELL_READ_CHUNK_BYTES)
            .read_until(b'\n', &mut bytes)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if tx.send(ShellLine::Data(bytes)).is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send(ShellLine::Closed);
}

#[derive(Debug, Clone, Copy)]
pub struct CollectedLogs {
    pub limited: bool,
}

pub async fn stream_output<O, E>(
    mut stdout: O,
    mut stderr: E,
    record: Arc<Mutex<RunRecord>>,
    max_live_chars: usize,
) -> io::Result<CollectedLogs>
where
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
{
    let paths = record.lock().logs.clone();
    let mut combined_sink = StreamSink::create(&paths.combined).await?;
    let mut stdout_sink = StreamSink::create(&paths.stdout).await?;
    let mut stderr_sink = StreamSink::create(&paths.stderr).await?;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_buffer = [0u8; 8192];
    let mut stderr_buffer = [0u8; 8192];
    let live_limit = max_live_chars
        .saturating_mul(4)
        .clamp(8 * 1024, LIVE_TAIL_MAX_BYTES);
    let mut live_tail = Vec::new();
    let mut total_seen = 0usize;

    while !stdout_done || !stderr_done {
        tokio::select! {
            read = stdout.read(&mut stdout_buffer), if !stdout_done => {
                let read = read?;
                if read == 0 {
                    stdout_done = true;
                } else {
                    let chunk = &stdout_buffer[..read];
                    combined_sink.push(chunk).await?;
                    stdout_sink.push(chunk).await?;
                    total_seen = total_seen.saturating_add(read);
                    append_rolling(&mut live_tail, chunk, live_limit);
                    update_live_preview(&record, &live_tail, total_seen > live_tail.len());
                }
            }
            read = stderr.read(&mut stderr_buffer), if !stderr_done => {
                let read = read?;
                if read == 0 {
                    stderr_done = true;
                } else {
                    let chunk = &stderr_buffer[..read];
                    combined_sink.push(chunk).await?;
                    stderr_sink.push(chunk).await?;
                    total_seen = total_seen.saturating_add(read);
                    append_rolling(&mut live_tail, chunk, live_limit);
                    update_live_preview(&record, &live_tail, total_seen > live_tail.len());
                }
            }
        }
    }

    let combined_limited = combined_sink.finish().await?;
    let stdout_limited = stdout_sink.finish().await?;
    let stderr_limited = stderr_sink.finish().await?;
    Ok(CollectedLogs {
        limited: combined_limited || stdout_limited || stderr_limited,
    })
}

fn update_live_preview(record: &Arc<Mutex<RunRecord>>, bytes: &[u8], truncated: bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut item = record.lock();
    item.output = strip_ansi(&text);
    item.output_truncated = truncated;
}

pub async fn render_preview(
    paths: &RunLogPaths,
    filter: &OutputFilter,
    status: &str,
    max_output: usize,
    log_limited: bool,
) -> (String, bool) {
    let combined = read_lossy(&paths.combined).await;
    let stdout = read_lossy(&paths.stdout).await;
    let stderr = read_lossy(&paths.stderr).await;
    let failed = matches!(status, "failed" | "timed_out" | "cancelled");

    let selected = match filter {
        OutputFilter::Raw => {
            if failed {
                tail_chars(&combined, max_output)
            } else {
                head_chars(&combined, max_output)
            }
        }
        OutputFilter::FailedTail { chars } => {
            let limit = (*chars).max(1).min(max_output);
            if failed {
                tail_chars(&combined, limit)
            } else {
                head_chars(&combined, limit)
            }
        }
        OutputFilter::TailLines { lines } => tail_lines(&combined, (*lines).max(1)),
        OutputFilter::CargoJson { include_warnings } => {
            cargo_summary(&stdout, &stderr, *include_warnings, status, max_output)
        }
        OutputFilter::JsonSummary { marker } => json_summary(&combined, marker, status, max_output),
    };

    let selected = strip_ansi(&selected);
    let raw_chars = combined.chars().count();
    let selected_chars = selected.chars().count();
    let filtered = !matches!(filter, OutputFilter::Raw);
    (
        selected,
        log_limited || filtered || selected_chars < raw_chars,
    )
}

async fn read_lossy(path: &Path) -> String {
    match tokio::fs::read(path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

fn head_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn tail_chars(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    value.chars().skip(count.saturating_sub(limit)).collect()
}

fn tail_lines(value: &str, count: usize) -> String {
    let lines: Vec<&str> = value.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

fn json_summary(combined: &str, marker: &str, status: &str, max_output: usize) -> String {
    if marker.is_empty() {
        return if matches!(status, "failed" | "timed_out" | "cancelled") {
            tail_chars(combined, max_output)
        } else {
            head_chars(combined, max_output)
        };
    }
    if let Some(index) = combined.rfind(marker) {
        let summary = combined[index + marker.len()..].trim();
        if !summary.is_empty() {
            return head_chars(summary, max_output);
        }
    }
    if matches!(status, "failed" | "timed_out" | "cancelled") {
        tail_chars(combined, max_output)
    } else {
        head_chars(combined, max_output)
    }
}

fn cargo_summary(
    stdout: &str,
    stderr: &str,
    include_warnings: bool,
    status: &str,
    max_output: usize,
) -> String {
    let mut diagnostics = Vec::new();
    let mut seen = HashSet::new();
    let mut build_success = None;
    let mut non_json = Vec::new();

    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            if !line.trim().is_empty() {
                non_json.push(line);
            }
            continue;
        };
        match value.get("reason").and_then(Value::as_str) {
            Some("build-finished") => {
                build_success = value.get("success").and_then(Value::as_bool);
            }
            Some("compiler-message") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                let level = message
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if level != "error" && !(include_warnings && level == "warning") {
                    continue;
                }
                let rendered = message
                    .get("rendered")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| compact_cargo_message(message, level));
                if seen.insert(rendered.clone()) {
                    diagnostics.push(rendered);
                }
            }
            _ => {}
        }
    }

    let mut output = String::new();
    let succeeded = status == "succeeded" && build_success.unwrap_or(true);
    output.push_str(if succeeded {
        "Cargo command succeeded"
    } else {
        "Cargo command failed"
    });
    output.push_str(&format!("; {} retained diagnostic(s).", diagnostics.len()));

    for diagnostic in diagnostics {
        output.push_str("\n\n");
        output.push_str(&diagnostic);
    }
    if !non_json.is_empty() {
        output.push_str("\n\nTest/program output:\n");
        output.push_str(&non_json[non_json.len().saturating_sub(80)..].join("\n"));
    }
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        output.push_str("\n\nCargo status:\n");
        output.push_str(stderr);
    }

    if succeeded {
        head_chars(&output, max_output)
    } else {
        tail_chars(&output, max_output)
    }
}

fn compact_cargo_message(message: &Value, level: &str) -> String {
    let text = message
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("compiler diagnostic");
    let primary = message
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| {
            spans
                .iter()
                .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
        });
    if let Some(span) = primary {
        let file = span
            .get("file_name")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let line = span.get("line_start").and_then(Value::as_u64).unwrap_or(0);
        let column = span
            .get("column_start")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        format!("{file}:{line}:{column}: {level}: {text}")
    } else {
        format!("{level}: {text}")
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
        let process_group = format!("-{pid}");
        let _ = std::process::Command::new("kill")
            .args(["-TERM", "--", &process_group])
            .status();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let process_group = format!("-{pid}");
            let still_running = std::process::Command::new("kill")
                .args(["-0", "--", &process_group])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if still_running {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", "--", &process_group])
                    .status();
            }
        });
    }
}

pub fn remove_logs(paths: &RunLogPaths) {
    for path in paths.all() {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("Bash log cleanup failed for {}: {error}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_buffer_keeps_the_tail() {
        let mut buffer = b"abcdef".to_vec();
        append_rolling(&mut buffer, b"ghij", 6);
        assert_eq!(buffer, b"efghij");
    }

    #[cfg(windows)]
    #[test]
    fn shell_paths_remove_windows_verbatim_prefixes() {
        assert_eq!(
            to_shell_path(Path::new(r"\\?\C:\Projects\codeweave")),
            "C:/Projects/codeweave"
        );
        assert_eq!(
            to_shell_path(Path::new(r"\\?\UNC\server\share\workspace")),
            "//server/share/workspace"
        );
    }

    #[test]
    fn cargo_filter_extracts_compiler_errors() {
        let stdout = r#"{"reason":"compiler-message","message":{"level":"error","message":"bad type","rendered":"error: bad type\n","spans":[]}}
{"reason":"build-finished","success":false}"#;
        let output = cargo_summary(stdout, "", false, "failed", 30_000);
        assert!(output.contains("bad type"));
        assert!(!output.contains("compiler-message"));
    }

    #[test]
    fn strips_csi_and_osc_sequences() {
        let value = "\u{1b}[32mok\u{1b}[0m \u{1b}]0;title\u{7}done";
        assert_eq!(strip_ansi(value), "ok done");
    }
}

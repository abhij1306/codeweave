//! JSON-RPC over a language server's stdio, framed with `Content-Length`.
use crate::process_runtime::terminate_process_tree;
#[cfg(windows)]
use crate::process_runtime::WindowsJob;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Largest JSON-RPC body accepted from a server.
pub(super) const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Bytes of server stderr retained for startup and crash diagnostics.
const STDERR_TAIL_BYTES: usize = 8 * 1024;
/// How long `Drop` waits for the reader threads after killing the server.
const READER_JOIN_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(super) enum TransportError {
    /// No message arrived before the timeout; the server may still answer.
    TimedOut,
    /// The server closed its output or exited; no further messages will arrive.
    Disconnected,
    /// The server sent a frame that cannot be accepted.
    Protocol(String),
    Io(io::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("language-server response timed out"),
            Self::Disconnected => formatter.write_str("language server closed its connection"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset => Self::Disconnected,
            io::ErrorKind::TimedOut => Self::TimedOut,
            _ => Self::Io(error),
        }
    }
}

pub(super) trait RpcTransport: Send {
    fn send(&mut self, value: &Value) -> Result<(), TransportError>;
    fn receive(&mut self, timeout: Duration) -> Result<Value, TransportError>;
    /// The most recent server stderr output, for diagnostics.
    fn stderr_tail(&self) -> Option<String> {
        None
    }
}

pub(super) type TransportFactory = Arc<dyn Fn() -> io::Result<Box<dyn RpcTransport>> + Send + Sync>;

enum ReaderEvent {
    Message(Value),
    Fatal(String),
}

pub(super) struct StdioRpcTransport {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<ReaderEvent>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    threads: Vec<JoinHandle<()>>,
    pid: u32,
    #[cfg(windows)]
    job: Option<Arc<WindowsJob>>,
}

impl StdioRpcTransport {
    pub(super) fn spawn(command: &str, args: &[String], cwd: &Path) -> io::Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        #[cfg(windows)]
        let job = WindowsJob::assign(pid).ok();
        let missing = |stream: &str| io::Error::other(format!("JSON-RPC {stream} unavailable"));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
        let (tx, events) = mpsc::channel();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let tail = Arc::clone(&stderr_tail);
        let spawned = thread::Builder::new()
            .name("codeweave-lsp-stdout".to_owned())
            .spawn(move || read_frames(BufReader::new(stdout), &tx))
            .and_then(|reader| {
                thread::Builder::new()
                    .name("codeweave-lsp-stderr".to_owned())
                    .spawn(move || retain_tail(stderr, &tail))
                    .map(|stderr| vec![reader, stderr])
            });
        let threads = match spawned {
            Ok(threads) => threads,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            stdin,
            events,
            stderr: stderr_tail,
            threads,
            pid,
            #[cfg(windows)]
            job,
        })
    }
}

/// Read `Content-Length` frames until EOF or an unrecoverable framing error.
fn read_frames(mut reader: impl BufRead, tx: &mpsc::Sender<ReaderEvent>) {
    loop {
        let event = match read_frame(&mut reader) {
            Ok(Some(body)) => match serde_json::from_slice(&body) {
                Ok(value) => ReaderEvent::Message(value),
                Err(error) => {
                    tracing::warn!(%error, bytes = body.len(), "dropping unparsable language-server frame");
                    continue;
                }
            },
            Ok(None) => return,
            Err(message) => {
                tracing::warn!(%message, "language-server output is unusable");
                let _ = tx.send(ReaderEvent::Fatal(message));
                return;
            }
        };
        if tx.send(event).is_err() {
            return;
        }
    }
}

/// One frame body, `Ok(None)` at EOF, or a fatal framing error.
fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut length = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Ok(None),
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            tracing::warn!(
                header = trimmed,
                "ignoring malformed language-server header"
            );
            continue;
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("language server sent an invalid Content-Length: {value}"))?;
            length = Some(parsed);
        }
    }
    let length = length.unwrap_or_default();
    if length > MAX_FRAME_BYTES {
        return Err(format!(
            "language-server frame of {length} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
        ));
    }
    let mut body = vec![0; length];
    match reader.read_exact(&mut body) {
        Ok(()) => Ok(Some(body)),
        Err(_) => Ok(None),
    }
}

fn retain_tail(mut stderr: impl Read, tail: &Mutex<VecDeque<u8>>) {
    let mut buffer = [0u8; 4096];
    while let Ok(count) = stderr.read(&mut buffer) {
        if count == 0 {
            return;
        }
        let mut tail = tail.lock();
        tail.extend(&buffer[..count]);
        let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
        tail.drain(..excess);
    }
}

impl RpcTransport for StdioRpcTransport {
    fn send(&mut self, value: &Value) -> Result<(), TransportError> {
        let body = serde_json::to_vec(value).map_err(|error| TransportError::Io(error.into()))?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Value, TransportError> {
        match self.events.recv_timeout(timeout) {
            Ok(ReaderEvent::Message(value)) => Ok(value),
            Ok(ReaderEvent::Fatal(message)) => Err(TransportError::Protocol(message)),
            Err(RecvTimeoutError::Timeout) => Err(TransportError::TimedOut),
            Err(RecvTimeoutError::Disconnected) => Err(TransportError::Disconnected),
        }
    }

    fn stderr_tail(&self) -> Option<String> {
        let tail = self.stderr.lock();
        (!tail.is_empty()).then(|| {
            let (front, back) = tail.as_slices();
            String::from_utf8_lossy(&[front, back].concat()).into_owned()
        })
    }
}

impl Drop for StdioRpcTransport {
    fn drop(&mut self) {
        terminate_process_tree(self.pid, {
            #[cfg(windows)]
            {
                self.job.as_deref()
            }
            #[cfg(not(windows))]
            {
                None
            }
        });
        let _ = self.child.wait();
        // The pipes close once the process tree is gone. A grandchild that
        // escaped the tree could keep them open, so never block on it.
        let deadline = Instant::now() + READER_JOIN_GRACE;
        for thread in self.threads.drain(..) {
            while !thread.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!("language-server pipe reader did not stop after shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(input: &[u8]) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        read_frames(io::Cursor::new(input.to_vec()), &tx);
        drop(tx);
        rx.iter()
            .map(|event| match event {
                ReaderEvent::Message(value) => value.to_string(),
                ReaderEvent::Fatal(message) => format!("fatal: {message}"),
            })
            .collect()
    }

    #[test]
    fn frames_are_decoded_and_unparsable_bodies_are_skipped() {
        let input = b"Content-Length: 7\r\n\r\n{\"a\":1}Content-Length: 3\r\n\r\nnopContent-Type: x\r\nContent-Length: 7\r\n\r\n{\"b\":2}";
        assert_eq!(frames(input), vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn oversized_frames_end_the_stream_with_a_fatal_error() {
        let input = format!("Content-Length: {}\r\n\r\n{{}}", MAX_FRAME_BYTES + 1);
        assert_eq!(
            frames(input.as_bytes()),
            vec![format!(
                "fatal: language-server frame of {} bytes exceeds the {MAX_FRAME_BYTES} byte limit",
                MAX_FRAME_BYTES + 1
            )]
        );
    }

    #[test]
    fn an_exited_server_is_disconnected_at_once_and_keeps_its_stderr() {
        let cwd = tempfile::tempdir().unwrap();
        let mut transport =
            StdioRpcTransport::spawn("git", &["--no-such-codeweave-flag".to_owned()], cwd.path())
                .unwrap();
        let started = Instant::now();

        let result = transport.receive(Duration::from_secs(60));

        assert!(
            matches!(result, Err(TransportError::Disconnected)),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        let mentions_flag = |transport: &StdioRpcTransport| {
            transport
                .stderr_tail()
                .is_some_and(|tail| tail.contains("no-such-codeweave-flag"))
        };
        let waited = Instant::now();
        while !mentions_flag(&transport) && waited.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(mentions_flag(&transport), "{:?}", transport.stderr_tail());
    }

    #[test]
    fn stderr_tail_keeps_only_the_most_recent_bytes() {
        let tail = Mutex::new(VecDeque::new());
        let mut output = vec![b'a'; STDERR_TAIL_BYTES];
        output.extend_from_slice(b"last line");
        retain_tail(io::Cursor::new(output), &tail);
        let tail = tail.lock();
        assert_eq!(tail.len(), STDERR_TAIL_BYTES);
        assert!(tail.iter().rev().take(9).rev().eq(b"last line".iter()));
    }
}

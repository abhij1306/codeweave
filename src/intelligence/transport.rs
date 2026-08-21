use crate::process_runtime::terminate_process_tree;
#[cfg(windows)]
use crate::process_runtime::WindowsJob;
use serde_json::Value;
use std::io;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub(super) trait RpcTransport: Send {
    fn send(&mut self, value: &Value) -> io::Result<()>;
    fn receive(&mut self, timeout: Duration) -> io::Result<Value>;
}

pub(super) struct StdioRpcTransport {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
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
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("JSON-RPC stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("JSON-RPC stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("JSON-RPC stderr unavailable"))?;
        let (tx, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if reader
                        .read_line(&mut line)
                        .ok()
                        .filter(|count| *count > 0)
                        .is_none()
                    {
                        return;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                        length = value.trim().parse::<usize>().ok();
                    }
                }
                let Some(length) = length else {
                    continue;
                };
                let mut body = vec![0; length];
                if std::io::Read::read_exact(&mut reader, &mut body).is_err() {
                    return;
                }
                if let Ok(value) = serde_json::from_slice(&body) {
                    if tx.send(value).is_err() {
                        return;
                    }
                }
            }
        });
        thread::spawn(move || {
            let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
        });
        Ok(Self {
            child,
            stdin,
            messages,
            pid,
            #[cfg(windows)]
            job,
        })
    }
}

impl RpcTransport for StdioRpcTransport {
    fn send(&mut self, value: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(value).map_err(io::Error::other)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Value> {
        self.messages
            .recv_timeout(timeout)
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))
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
    }
}

pub(super) type TransportFactory = Arc<dyn Fn() -> io::Result<Box<dyn RpcTransport>> + Send + Sync>;

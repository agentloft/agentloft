//! Child process supervision (PLAN §5.4): every child runs in its own process
//! group so the whole tree can be signalled; stderr is kept as a bounded tail.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

/// Configure `cmd` to start a new process group and die with its handle.
pub fn configure_group(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.kill_on_drop(true);
}

/// Send `signal` to the process group led by `pid`.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn signal_group(pid: u32, signal: i32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with a negative pid targets the process group; no memory
    // is shared with the callee.
    unsafe { libc::kill(-pid, signal) == 0 }
}

#[cfg(not(unix))]
pub fn signal_group(_pid: u32, _signal: i32) -> bool {
    false
}

#[cfg(unix)]
pub const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
pub const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
pub const SIGTERM: i32 = 15;
#[cfg(not(unix))]
pub const SIGKILL: i32 = 9;

/// Graceful stop: the caller has already closed stdin. Wait `grace`, then
/// SIGTERM the group, wait `term_grace`, then SIGKILL.
pub async fn stop_child(child: &mut Child, grace: Duration, term_grace: Duration) {
    if tokio::time::timeout(grace, child.wait()).await.is_ok() {
        return;
    }
    if let Some(pid) = child.id() {
        signal_group(pid, SIGTERM);
    }
    if tokio::time::timeout(term_grace, child.wait()).await.is_ok() {
        return;
    }
    if let Some(pid) = child.id() {
        signal_group(pid, SIGKILL);
    }
    let _ = child.kill().await;
}

/// Bounded tail of a child's stderr.
#[derive(Clone, Default)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    const MAX_LINES: usize = 60;

    /// Drain `stream` into the tail (and tracing) until EOF.
    pub fn collect<R: AsyncRead + Unpin + Send + 'static>(&self, stream: R, label: String) {
        let lines = self.lines.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(target: "harness.stderr", %label, "{line}");
                let mut buf = lines.lock().unwrap_or_else(|e| e.into_inner());
                if buf.len() == Self::MAX_LINES {
                    buf.pop_front();
                }
                buf.push_back(line);
            }
        });
    }

    pub fn snapshot(&self) -> String {
        let buf = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

//! Daemon-owned terminals (ADR-0007): PTYs via portable-pty, a scrollback
//! ring buffer per terminal for replay, and output fanned out on the bus
//! with byte offsets so subscribers never see duplicates.

use crate::BusMsg;
use loft_protocol::{ErrorBody, ErrorCode, TerminalId, TerminalInfo};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Scrollback kept per terminal for replay (8 MiB).
const RING_CAP: usize = 8 * 1024 * 1024;

struct Ring {
    data: VecDeque<u8>,
    /// Total bytes ever written; `end - data.len()` is the ring's start.
    end: u64,
}

struct Term {
    info: Mutex<TerminalInfo>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    ring: Mutex<Ring>,
}

#[derive(Default)]
pub struct Terminals {
    map: Mutex<HashMap<TerminalId, Arc<Term>>>,
    /// Client-chosen keys (e.g. one per thread) → terminal, for reattach.
    keys: Mutex<HashMap<String, TerminalId>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn err(msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, msg)
}

impl Terminals {
    pub fn open(
        &self,
        cwd: &Path,
        cols: u16,
        rows: u16,
        key: Option<String>,
        env: &HashMap<String, String>,
        bus: broadcast::Sender<BusMsg>,
    ) -> Result<TerminalInfo, ErrorBody> {
        let shell = env
            .get("SHELL")
            .cloned()
            .unwrap_or_else(|| "/bin/zsh".into());
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: rows.max(2),
                cols: cols.max(2),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| err(format!("openpty: {e}")))?;
        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-l");
        cmd.cwd(cwd);
        for (k, v) in env {
            if !matches!(k.as_str(), "CLAUDECODE" | "CLAUDE_CODE_ENTRYPOINT") {
                cmd.env(k, v);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Agentloft");
        if !env.contains_key("LANG") {
            cmd.env("LANG", "en_US.UTF-8");
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| err(format!("spawn {shell}: {e}")))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| err(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| err(format!("pty writer: {e}")))?;

        let id = TerminalId::new();
        let info = TerminalInfo {
            id: id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            shell: shell.clone(),
            cols,
            rows,
            exited: None,
        };
        let term = Arc::new(Term {
            info: Mutex::new(info.clone()),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(child.clone_killer()),
            ring: Mutex::new(Ring {
                data: VecDeque::new(),
                end: 0,
            }),
        });
        lock(&self.map).insert(id.clone(), term.clone());
        // A terminal previously under this key has exited: drop it.
        if let Some(key) = key
            && let Some(old) = lock(&self.keys).insert(key, id.clone())
            && let Some(old) = lock(&self.map).remove(&old)
        {
            let _ = lock(&old.killer).kill();
        }

        // Reader thread: PTY → ring buffer → bus.
        std::thread::Builder::new()
            .name(format!("pty-{}", &id.as_str()[..8]))
            .spawn(move || {
                pump(reader, &term, &id, &bus);
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                lock(&term.info).exited = Some(code.unwrap_or(-1));
                let _ = bus.send(BusMsg::TerminalExited { id, code });
            })
            .map_err(|e| err(format!("spawn reader: {e}")))?;
        Ok(info)
    }

    /// The live terminal opened under `key`, resized to the caller's grid.
    pub fn reattach(&self, key: &str, cols: u16, rows: u16) -> Option<TerminalInfo> {
        let id = lock(&self.keys).get(key).cloned()?;
        let term = self.get(&id).ok()?;
        if lock(&term.info).exited.is_some() {
            return None;
        }
        // 0×0 attaches without resizing (e.g. a CLI peeking at a UI's shell).
        if cols > 0 && rows > 0 {
            self.resize(&id, cols, rows).ok()?;
        }
        let info = lock(&term.info).clone();
        Some(info)
    }

    fn get(&self, id: &TerminalId) -> Result<Arc<Term>, ErrorBody> {
        lock(&self.map)
            .get(id)
            .cloned()
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "terminal not found"))
    }

    pub fn input(&self, id: &TerminalId, data: &[u8]) -> Result<(), ErrorBody> {
        let term = self.get(id)?;
        let mut w = lock(&term.writer);
        w.write_all(data)
            .and_then(|_| w.flush())
            .map_err(|e| err(format!("write: {e}")))
    }

    pub fn resize(&self, id: &TerminalId, cols: u16, rows: u16) -> Result<(), ErrorBody> {
        let term = self.get(id)?;
        lock(&term.master)
            .resize(PtySize {
                rows: rows.max(2),
                cols: cols.max(2),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| err(format!("resize: {e}")))?;
        let mut info = lock(&term.info);
        info.cols = cols;
        info.rows = rows;
        Ok(())
    }

    pub fn close(&self, id: &TerminalId) -> Result<(), ErrorBody> {
        let term = lock(&self.map).remove(id);
        lock(&self.keys).retain(|_, t| t != id);
        if let Some(term) = term {
            let _ = lock(&term.killer).kill();
        }
        Ok(())
    }

    /// Close the terminal opened under `key`, if any.
    pub fn close_key(&self, key: &str) {
        let id = lock(&self.keys).get(key).cloned();
        if let Some(id) = id {
            let _ = self.close(&id);
        }
    }

    /// Info plus the replay buffer and the byte offset where it ends.
    pub fn snapshot(&self, id: &TerminalId) -> Result<(TerminalInfo, Vec<u8>, u64), ErrorBody> {
        let term = self.get(id)?;
        let info = lock(&term.info).clone();
        let ring = lock(&term.ring);
        let (a, b) = ring.data.as_slices();
        Ok((info, [a, b].concat(), ring.end))
    }

    pub fn shutdown(&self) {
        for (_, term) in lock(&self.map).drain() {
            let _ = lock(&term.killer).kill();
        }
    }
}

fn pump(
    mut reader: Box<dyn Read + Send>,
    term: &Term,
    id: &TerminalId,
    bus: &broadcast::Sender<BusMsg>,
) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let chunk = buf[..n].to_vec();
        let offset = {
            let mut ring = lock(&term.ring);
            let offset = ring.end;
            ring.data.extend(&chunk);
            let excess = ring.data.len().saturating_sub(RING_CAP);
            ring.data.drain(..excess);
            ring.end += n as u64;
            offset
        };
        let _ = bus.send(BusMsg::TerminalOutput {
            id: id.clone(),
            offset,
            data: Arc::new(chunk),
        });
    }
}

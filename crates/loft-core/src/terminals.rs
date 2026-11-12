//! Daemon-owned terminals (ADR-0007): PTYs via portable-pty, a scrollback
//! ring buffer per terminal for replay, and output fanned out on the bus
//! with byte offsets so subscribers never see duplicates.

use crate::BusMsg;
use loft_protocol::{ErrorBody, ErrorCode, TerminalId, TerminalInfo};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Scrollback kept per terminal for replay (8 MiB).
const RING_CAP: usize = 8 * 1024 * 1024;

/// A new shell is ready for a typed command once it has printed something
/// (its prompt) and then nothing for this long…
const PROMPT_QUIET: Duration = Duration::from_millis(200);
/// …or after this long whatever it printed: a shell reads what was typed
/// ahead once it gets there.
const PROMPT_WAIT: Duration = Duration::from_secs(5);
/// A shell that was just typed a command takes a moment to put it in the
/// foreground; until then it still looks idle, and isn't.
const STARTING: Duration = Duration::from_millis(600);

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
    /// The shell's process id: it is in the foreground while it waits at
    /// its prompt.
    shell_pid: Option<u32>,
    /// A command is waiting for the new shell's prompt to be typed.
    typing: AtomicBool,
    /// When a command was last typed for the user.
    typed_at: Mutex<Option<Instant>>,
}

impl Term {
    /// Whether the shell is waiting at its prompt: no command of its own
    /// is in the terminal's foreground. `false` where that can't be told.
    fn idle(&self) -> bool {
        if self.typing.load(Ordering::SeqCst)
            || lock(&self.info).exited.is_some()
            || lock(&self.typed_at).is_some_and(|at| at.elapsed() < STARTING)
        {
            return false;
        }
        #[cfg(unix)]
        {
            let foreground = lock(&self.master).process_group_leader();
            foreground.is_some() && foreground == self.shell_pid.map(|pid| pid as _)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Type `command` and Enter, as the user would.
    fn type_line(&self, command: &str) {
        *lock(&self.typed_at) = Some(Instant::now());
        let mut w = lock(&self.writer);
        let _ = w
            .write_all(command.trim().as_bytes())
            .and_then(|_| w.write_all(b"\r"))
            .and_then(|_| w.flush());
    }
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
        (cols, rows): (u16, u16),
        key: Option<String>,
        command: Option<String>,
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
            key: key.clone(),
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
            shell_pid: child.process_id(),
            typing: AtomicBool::new(command.is_some()),
            typed_at: Mutex::new(None),
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
        let typist = command.map(|command| (term.clone(), command));
        std::thread::Builder::new()
            .name(format!("pty-{}", &id.as_str()[..8]))
            .spawn(move || {
                pump(reader, &term, &id, &bus);
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                lock(&term.info).exited = Some(code.unwrap_or(-1));
                let _ = bus.send(BusMsg::TerminalExited { id, code });
            })
            .map_err(|e| err(format!("spawn reader: {e}")))?;
        if let Some((term, command)) = typist {
            let typed = std::thread::Builder::new()
                .name("pty-command".into())
                .spawn({
                    let term = term.clone();
                    move || type_at_prompt(&term, &command)
                });
            if typed.is_err() {
                term.typing.store(false, Ordering::SeqCst);
            }
        }
        Ok(info)
    }

    /// Type `command` into a live terminal if its shell is waiting at the
    /// prompt (nothing of its own running); says whether it did.
    pub fn run_if_idle(&self, id: &TerminalId, command: &str) -> bool {
        let Ok(term) = self.get(id) else {
            return false;
        };
        let idle = term.idle();
        if idle {
            term.type_line(command);
        }
        idle
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

    /// Close every terminal whose key starts with `prefix`.
    pub fn close_prefix(&self, prefix: &str) {
        let ids: Vec<TerminalId> = lock(&self.keys)
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, id)| id.clone())
            .collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    /// Whether a live terminal whose key starts with `prefix` is running
    /// something (its shell isn't waiting at the prompt).
    pub fn busy_prefix(&self, prefix: &str) -> bool {
        let ids: Vec<TerminalId> = lock(&self.keys)
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, id)| id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| self.get(id).ok())
            .any(|t| lock(&t.info).exited.is_none() && !t.idle())
    }

    /// Live terminals whose key starts with `prefix`, ordered by key.
    pub fn list_prefix(&self, prefix: &str) -> Vec<TerminalInfo> {
        let mut found: Vec<(String, TerminalId)> = lock(&self.keys)
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, id)| (k.clone(), id.clone()))
            .collect();
        found.sort();
        found
            .into_iter()
            .filter_map(|(_, id)| self.get(&id).ok())
            .map(|t| lock(&t.info).clone())
            .filter(|info| info.exited.is_none())
            .collect()
    }

    /// Bytes from stream offset `from` to the end, and that end. `None` if
    /// `from` is older than the ring's start (the reader fell behind).
    pub fn read_since(
        &self,
        id: &TerminalId,
        from: u64,
    ) -> Result<Option<(Vec<u8>, u64)>, ErrorBody> {
        let term = self.get(id)?;
        let ring = lock(&term.ring);
        let start = ring.end - ring.data.len() as u64;
        if from < start {
            return Ok(None);
        }
        let skip = (from - start) as usize;
        let bytes = ring
            .data
            .range(skip.min(ring.data.len())..)
            .copied()
            .collect();
        Ok(Some((bytes, ring.end)))
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

/// Type `command` into a new shell once its prompt is up. Typed any
/// sooner the shell would still run it, but the line would show twice:
/// echoed by the terminal as it arrives, then drawn again by the shell's
/// own line editor.
fn type_at_prompt(term: &Term, command: &str) {
    let started = Instant::now();
    let (mut seen, mut changed) = (0, started);
    loop {
        std::thread::sleep(Duration::from_millis(25));
        if lock(&term.info).exited.is_some() {
            break;
        }
        let end = lock(&term.ring).end;
        if end != seen {
            (seen, changed) = (end, Instant::now());
        }
        if (seen > 0 && changed.elapsed() >= PROMPT_QUIET) || started.elapsed() >= PROMPT_WAIT {
            term.type_line(command);
            break;
        }
    }
    term.typing.store(false, Ordering::SeqCst);
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
        let end = {
            let mut ring = lock(&term.ring);
            ring.data.extend(&buf[..n]);
            let excess = ring.data.len().saturating_sub(RING_CAP);
            ring.data.drain(..excess);
            ring.end += n as u64;
            ring.end
        };
        let _ = bus.send(BusMsg::TerminalOutput {
            id: id.clone(),
            end,
        });
    }
}

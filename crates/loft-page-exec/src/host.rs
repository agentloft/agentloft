//! The async side of a run: runs the commands and sleeps the engine asks
//! for, keeps the clock, the console and the pictures, and writes the
//! report.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use loft_page_script::BrowserCommand;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use crate::engine::{self, End, Head, Job, ToEngine, ToHost};
use crate::{
    Image, MAX_IMAGE_BYTES, MAX_IMAGES, MAX_RESULT_CHARS, Report, Script, TabInfo, Tabs, cut,
};

/// Pictures a run keeps (taken by commands, whether returned or not).
const MAX_PICTURES: usize = 32;
/// The key a kept picture's index travels under, in a command's result.
const IMAGE_KEY: &str = "__loft_image";
const MAX_CONSOLE_LINES: usize = 200;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_CONSOLE_CHARS: usize = 20_000;
const MAX_ERROR_CHARS: usize = 3_000;
const MAX_ADDRESS_CHARS: usize = 200;
/// The longest a timer may wait.
const MAX_SLEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a stopped engine has to hand back its state.
const GRACE: Duration = Duration::from_millis(500);

/// Stops the engine when the run's future is dropped (or finished).
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

type Pending<'a> = Pin<Box<dyn Future<Output = (u64, Settled)> + Send + 'a>>;

enum Settled {
    Call {
        action: &'static str,
        result: Result<Value, String>,
    },
    Sleep,
}

#[derive(Default)]
struct Record {
    console: Vec<String>,
    dropped_lines: usize,
    host_warnings: Vec<String>,
    script_warnings: Vec<String>,
    pictures: Vec<Image>,
    pictures_dropped: bool,
    navigations: usize,
    handoffs: usize,
}

impl Record {
    fn log(&mut self, level: &str, text: &str) {
        if self.console.len() >= MAX_CONSOLE_LINES {
            self.dropped_lines += 1;
            return;
        }
        let text = cut(text, MAX_LINE_CHARS);
        self.console.push(if level == "log" {
            text
        } else {
            format!("[{level}] {text}")
        });
    }

    fn host_warning(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !self.host_warnings.contains(&text) && !self.script_warnings.contains(&text) {
            self.host_warnings.push(text);
        }
    }

    fn script_warning(&mut self, text: String) {
        if !self.host_warnings.contains(&text) && !self.script_warnings.contains(&text) {
            self.script_warnings.push(text);
        }
    }

    /// A command's picture is kept here; the script gets its size and an
    /// index to ask for it by.
    fn keep_picture(&mut self, value: &mut Value) {
        let Some(object) = value.as_object_mut() else {
            return;
        };
        let is_picture = object
            .get("mime_type")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("image/"))
            && object.get("data").is_some_and(Value::is_string);
        if !is_picture {
            return;
        }
        let Some(Value::String(data)) = object.remove("data") else {
            return;
        };
        object.insert("size".into(), Value::from(data.len() * 3 / 4));
        if self.pictures.len() >= MAX_PICTURES {
            if !self.pictures_dropped {
                self.pictures_dropped = true;
                self.host_warning(format!(
                    "only the first {MAX_PICTURES} pictures of a script are kept; later ones were dropped"
                ));
            }
            return;
        }
        let mime_type = object
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_owned();
        object.insert(IMAGE_KEY.into(), Value::from(self.pictures.len()));
        self.pictures.push(Image { mime_type, data });
    }
}

pub(crate) async fn run(script: Script, tabs: &dyn Tabs) -> Report {
    let started_at = tabs.info(&script.tab);
    let cancel = Arc::new(AtomicBool::new(false));
    let _stop_engine = CancelOnDrop(cancel.clone());
    let (to_host, from_engine) = tokio::sync::mpsc::unbounded_channel();
    let (to_engine, engine_inbox) = std::sync::mpsc::channel();
    let job = Job {
        code: script.code,
        tab: script.tab.clone(),
        state: script.state,
    };
    let mut record = Record::default();
    let mut timed_out = false;
    let end = match engine::spawn(job, to_host, engine_inbox, cancel.clone()) {
        Err(error) => Some(End {
            head: Head::Stopped(format!("the JavaScript engine could not start: {error}")),
            images: Vec::new(),
            state: None,
            notes: Vec::new(),
        }),
        Ok(()) => {
            let mut host = Host {
                tabs,
                tab: &script.tab,
                record: &mut record,
                to_engine,
                from_engine,
                cancel,
            };
            let end = host.drive(script.timeout).await;
            timed_out = end.is_err();
            match end {
                Ok(end) => end,
                Err(late) => late,
            }
        }
    };
    report(
        &script.tab,
        script.timeout,
        tabs,
        started_at,
        record,
        end,
        timed_out,
    )
}

struct Host<'a, 'r> {
    tabs: &'a dyn Tabs,
    tab: &'a str,
    record: &'r mut Record,
    to_engine: std::sync::mpsc::Sender<ToEngine>,
    from_engine: UnboundedReceiver<ToHost>,
    cancel: Arc<AtomicBool>,
}

impl<'a> Host<'a, '_> {
    /// Until the engine is done (`Ok`), or the time is up (`Err`, with what
    /// the engine handed back in its grace period, if anything).
    async fn drive(&mut self, timeout: Duration) -> Result<Option<End>, Option<End>> {
        let mut pending: FuturesUnordered<Pending<'a>> = FuturesUnordered::new();
        let mut deadline = Instant::now() + timeout;
        // Handoffs and demonstrations in flight: the clock stops for people.
        let mut people = 0usize;
        let mut paused_at = Instant::now();
        loop {
            let clock = tokio::time::sleep_until(deadline);
            tokio::select! {
                message = self.from_engine.recv() => match message {
                    None => return Ok(None),
                    Some(ToHost::Done(end)) => return Ok(Some(end)),
                    Some(ToHost::Call { id, tab, command }) => {
                        let action = command.action();
                        if matches!(command, BrowserCommand::Handoff { .. } | BrowserCommand::Demonstrate { .. }) {
                            if people == 0 {
                                paused_at = Instant::now();
                            }
                            people += 1;
                        }
                        let tabs = self.tabs;
                        let tab = tab.unwrap_or_else(|| self.tab.to_owned());
                        pending.push(Box::pin(async move {
                            let result = tabs.call(&tab, command).await;
                            (id, Settled::Call { action, result })
                        }));
                    }
                    Some(ToHost::Sleep { id, ms }) => {
                        let wait = if ms.is_finite() && ms > 0.0 {
                            Duration::from_secs_f64(ms / 1000.0).min(MAX_SLEEP)
                        } else {
                            Duration::ZERO
                        };
                        pending.push(Box::pin(async move {
                            tokio::time::sleep(wait).await;
                            (id, Settled::Sleep)
                        }));
                    }
                    Some(other) => self.answer(other),
                },
                Some((id, settled)) = pending.next(), if !pending.is_empty() => {
                    let result = match settled {
                        Settled::Sleep => Ok(Value::Null),
                        Settled::Call { action, result } => {
                            if matches!(action, "handoff" | "demonstrate") {
                                people = people.saturating_sub(1);
                                if people == 0 {
                                    deadline += paused_at.elapsed();
                                }
                            }
                            self.settled(action, result)
                        }
                    };
                    let _ = self.to_engine.send(ToEngine::Settled { id, result });
                }
                () = clock, if people == 0 => {
                    return Err(self.stop().await);
                }
            }
        }
    }

    /// A command's answer: counted, its picture kept.
    fn settled(&mut self, action: &str, result: Result<Value, String>) -> Result<Value, String> {
        let mut value = result?;
        let navigated = matches!(action, "navigate" | "back" | "forward" | "reload")
            || value.get("navigated").and_then(Value::as_bool) == Some(true);
        if navigated {
            self.record.navigations += 1;
        }
        if matches!(action, "handoff" | "demonstrate") {
            self.record.handoffs += 1;
        }
        self.record.keep_picture(&mut value);
        Ok(value)
    }

    /// What doesn't wait: tab facts, pictures, console lines, warnings.
    fn answer(&mut self, message: ToHost) {
        match message {
            ToHost::Info { tab, reply } => {
                let _ = reply.send(self.tabs.info(&tab));
            }
            ToHost::Picture { index, reply } => {
                let data = self.record.pictures.get(index).map(|p| p.data.clone());
                let _ = reply.send(data);
            }
            ToHost::Log { level, text } => self.record.log(&level, &text),
            ToHost::Warn(text) => self.record.script_warning(text),
            // Commands and sleeps after the time is up are dropped.
            ToHost::Call { .. } | ToHost::Sleep { .. } | ToHost::Done(_) => {}
        }
    }

    /// Time is up: stop the engine and give it a moment to hand back its
    /// state (and its last console lines).
    async fn stop(&mut self) -> Option<End> {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.to_engine.send(ToEngine::Stop);
        let grace = tokio::time::sleep(GRACE);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                message = self.from_engine.recv() => match message {
                    None => return None,
                    Some(ToHost::Done(end)) => return Some(end),
                    Some(other) => self.answer(other),
                },
                () = &mut grace => return None,
            }
        }
    }
}

fn report(
    tab: &str,
    timeout: Duration,
    tabs: &dyn Tabs,
    started_at: Option<TabInfo>,
    mut record: Record,
    end: Option<End>,
    timed_out: bool,
) -> Report {
    let mut is_error = true;
    let mut images = Vec::new();
    let mut returned = 0usize;
    let mut state = None;
    let head = match &end {
        _ if timed_out => format!(
            "Error: the script timed out after {} ms. Give it a larger timeout_ms, or split the work over several calls",
            timeout.as_millis()
        ),
        None => "Error: the script ended without a result".to_owned(),
        Some(end) => match &end.head {
            Head::Stopped(message) => format!("Error: {message}"),
            Head::Threw(text) => cut(text, MAX_ERROR_CHARS),
            Head::Value(text) => {
                is_error = false;
                let length = text.chars().count();
                if length > MAX_RESULT_CHARS {
                    record.host_warning(format!(
                        "the result is {length} characters, more than the {MAX_RESULT_CHARS} shown: return only what you need"
                    ));
                }
                cut(text, MAX_RESULT_CHARS)
            }
        },
    };
    if let Some(end) = end {
        for note in &end.notes {
            record.host_warning(note.clone());
        }
        state = end.state;
        if !timed_out {
            let mut seen = HashSet::new();
            let wanted: Vec<usize> = end
                .images
                .iter()
                .copied()
                .filter(|i| *i < record.pictures.len() && seen.insert(*i))
                .collect();
            returned = wanted.len();
            let mut bytes = 0usize;
            for &index in &wanted {
                let picture = &record.pictures[index];
                if images.len() >= MAX_IMAGES || bytes + picture.data.len() > MAX_IMAGE_BYTES {
                    record.host_warning(format!(
                        "only {} of the {} returned pictures are attached (at most {MAX_IMAGES} and {} MB a result)",
                        images.len(),
                        wanted.len(),
                        MAX_IMAGE_BYTES / (1024 * 1024)
                    ));
                    break;
                }
                bytes += picture.data.len();
                images.push(Image {
                    mime_type: picture.mime_type.clone(),
                    data: picture.data.clone(),
                });
            }
        }
    }

    let mut sections = vec![format!("tab: {tab}"), head];
    let warnings: Vec<&String> = record
        .host_warnings
        .iter()
        .chain(record.script_warnings.iter())
        .collect();
    if !warnings.is_empty() {
        let mut text = "Warnings:".to_owned();
        for warning in warnings {
            text.push_str("\n- ");
            text.push_str(warning);
        }
        sections.push(text);
    }

    let mut happened = Vec::new();
    let ended_at = tabs.info(tab);
    let address = |url: &str| {
        if url.is_empty() {
            "a blank tab".to_owned()
        } else {
            cut(url, MAX_ADDRESS_CHARS)
        }
    };
    match (&started_at, &ended_at) {
        (Some(_), None) => happened.push("the tab closed".to_owned()),
        (Some(from), Some(to)) if from.url != to.url => happened.push(format!(
            "moved from {} to {}",
            address(&from.url),
            address(&to.url)
        )),
        _ => {}
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if record.navigations > 0 {
        happened.push(plural(record.navigations, "navigation", "navigations"));
    }
    if record.handoffs > 0 {
        happened.push(plural(record.handoffs, "handoff", "handoffs"));
    }
    let unseen = record.pictures.len().saturating_sub(returned);
    if unseen > 0 {
        happened.push(format!(
            "{} not returned (return one to see it)",
            plural(unseen, "picture", "pictures")
        ));
    }
    if !happened.is_empty() {
        sections.push(format!("Page: {}", happened.join("; ")));
    }

    if !record.console.is_empty() || record.dropped_lines > 0 {
        let mut text = record.console.join("\n");
        if record.dropped_lines > 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "… {} more lines not shown (at most {MAX_CONSOLE_LINES} a run)",
                record.dropped_lines
            ));
        }
        sections.push(format!("Console:\n{}", cut(&text, MAX_CONSOLE_CHARS)));
    }

    Report {
        tab: tab.to_owned(),
        text: sections.join("\n\n"),
        is_error,
        images,
        state,
    }
}

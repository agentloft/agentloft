//! The JavaScript side of a run: one QuickJS runtime on a thread of its own.
//!
//! The engine runs the script's jobs until it has nothing left to run, then
//! blocks on the host for the next thing that settles (a command's answer,
//! a timer). It never waits on anything itself, so a run's only waits are
//! the host's, and the host's clock is what stops a run that takes too long.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Instant;

use loft_page_script::BrowserCommand;
use rquickjs::context::EvalOptions;
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Function, IntoJs, Object, Persistent, Promise, Runtime, Value};
use serde_json::Value as Json;
use tokio::sync::mpsc::UnboundedSender;

use crate::{CPU_SLICE, MEMORY_LIMIT, TabInfo, mapping};

/// The engine's own stack limit: deep recursion ends in its RangeError.
const STACK_LIMIT: usize = 1024 * 1024;
/// The native stack of the engine's thread, well above [`STACK_LIMIT`], so
/// that the engine's limit, not the OS, is what stops a recursion.
const THREAD_STACK: usize = 8 * 1024 * 1024;

/// What the script's function is given, in this order: its globals.
const PARAMS: &str =
    "console, browser, context, page, state, browse, files, snapshot, ref, handoff, demonstrate";

const PRELUDE: &str = include_str!("prelude.js");

pub(crate) const CPU_MESSAGE: &str = "the script's synchronous JavaScript ran for more than 2 seconds without awaiting anything, so it was stopped. Break long loops up with `await` (every page call awaits), or do heavy work in the page with page.evaluate()";
const NEVER_MESSAGE: &str = "the script is waiting on a promise that nothing will ever settle (no page call or timer is pending)";
const OUT_OF_MEMORY: &str = "Error: the script most likely ran out of memory (a script has 64 MB; keep large data in the page and return only what you need)";

/// The engine asks the host.
pub(crate) enum ToHost {
    /// Run a command; its answer comes back as [`ToEngine::Settled`] `id`.
    Call {
        id: u64,
        tab: Option<String>,
        command: BrowserCommand,
    },
    /// Settle `id` after `ms` milliseconds.
    Sleep {
        id: u64,
        ms: f64,
    },
    /// A tab's address and title, at once.
    Info {
        tab: String,
        reply: SyncSender<Option<TabInfo>>,
    },
    /// A kept picture's base64 data, at once.
    Picture {
        index: usize,
        reply: SyncSender<Option<String>>,
    },
    Log {
        level: String,
        text: String,
    },
    Warn(String),
    Done(End),
}

/// The host answers.
pub(crate) enum ToEngine {
    Settled {
        id: u64,
        result: Result<Json, String>,
    },
    /// Stop now (the overall time limit), handing back the state.
    Stop,
}

/// What the script came to.
pub(crate) enum Head {
    /// It returned this (as text).
    Value(String),
    /// It threw this (as text).
    Threw(String),
    /// The engine stopped it.
    Stopped(String),
}

pub(crate) struct End {
    pub head: Head,
    /// Kept pictures the value returned, in order.
    pub images: Vec<usize>,
    pub state: Option<String>,
    /// The engine's own warnings (state that couldn't be kept, …).
    pub notes: Vec<String>,
}

impl End {
    fn stopped(message: impl Into<String>) -> Self {
        Self {
            head: Head::Stopped(message.into()),
            images: Vec::new(),
            state: None,
            notes: Vec::new(),
        }
    }
}

pub(crate) struct Job {
    pub code: String,
    pub tab: String,
    pub state: String,
}

/// Start the engine's thread. It always ends with [`ToHost::Done`] (unless
/// the host is gone by then).
pub(crate) fn spawn(
    job: Job,
    to_host: UnboundedSender<ToHost>,
    from_host: Receiver<ToEngine>,
    cancel: Arc<AtomicBool>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("loft-script".into())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            #[cfg(test)]
            let code = job.code.clone();
            let end = run(job, to_host.clone(), &from_host, cancel);
            let _ = to_host.send(ToHost::Done(end));
            #[cfg(test)]
            ENDED.lock().unwrap().push(code);
        })?;
    Ok(())
}

/// The scripts whose engine thread has ended, for tests.
#[cfg(test)]
pub(crate) static ENDED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cpu,
    Cancelled,
}

/// The CPU slice and why the engine was stopped, shared with QuickJS's
/// interrupt handler.
struct Clock {
    slice_end: Cell<Instant>,
    /// Saving `state` after a stop: a fresh slice, and the cancel flag
    /// doesn't apply.
    saving: Cell<bool>,
    stop: Cell<Option<Stop>>,
    cancel: Arc<AtomicBool>,
}

impl Clock {
    fn restart(&self) {
        self.slice_end.set(Instant::now() + CPU_SLICE);
    }

    /// The interrupt handler: `true` stops the JavaScript running now.
    fn interrupt(&self) -> bool {
        let saving = self.saving.get();
        if self.stop.get().is_some() && !saving {
            return true;
        }
        if !saving && self.cancel.load(Ordering::Relaxed) {
            self.stop.set(Some(Stop::Cancelled));
            return true;
        }
        if Instant::now() > self.slice_end.get() {
            if !saving {
                self.stop.set(Some(Stop::Cpu));
            }
            return true;
        }
        false
    }

    fn stopped(&self) -> bool {
        self.stop.get().is_some() || (!self.saving.get() && self.cancel.load(Ordering::Relaxed))
    }
}

/// What the natives share.
struct Shared {
    to_host: UnboundedSender<ToHost>,
    clock: Rc<Clock>,
    /// Commands and sleeps the host may still settle.
    outstanding: RefCell<HashSet<u64>>,
    next_id: Cell<u64>,
    /// The host stopped listening.
    gone: Cell<bool>,
}

impl Shared {
    fn send(&self, message: ToHost) {
        if self.to_host.send(message).is_err() {
            self.gone.set(true);
        }
    }

    fn start(&self, message: impl FnOnce(u64) -> ToHost) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        self.outstanding.borrow_mut().insert(id);
        self.send(message(id));
        id
    }

    /// A question the host answers at once; control comes back with a
    /// fresh CPU slice.
    fn ask<T>(&self, message: impl FnOnce(SyncSender<T>) -> ToHost) -> Option<T> {
        let (reply, answer) = sync_channel(1);
        self.send(message(reply));
        let answer = answer.recv().ok();
        if answer.is_none() {
            self.gone.set(true);
        }
        self.clock.restart();
        answer
    }
}

type Rejections = Rc<RefCell<Vec<(Persistent<Value<'static>>, Persistent<Value<'static>>)>>>;

fn run(
    job: Job,
    to_host: UnboundedSender<ToHost>,
    from_host: &Receiver<ToEngine>,
    cancel: Arc<AtomicBool>,
) -> End {
    let Ok(runtime) = Runtime::new() else {
        return End::stopped("the JavaScript engine could not start");
    };
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let clock = Rc::new(Clock {
        slice_end: Cell::new(Instant::now() + CPU_SLICE),
        saving: Cell::new(false),
        stop: Cell::new(None),
        cancel,
    });
    runtime.set_interrupt_handler(Some(Box::new({
        let clock = clock.clone();
        move || clock.interrupt()
    })));
    // Rejections nobody handled (yet): (promise, reason).
    let rejections: Rejections = Rc::default();
    runtime.set_host_promise_rejection_tracker(Some(Box::new({
        let rejections = rejections.clone();
        move |ctx, promise, reason, handled| {
            let mut list = rejections.borrow_mut();
            if handled {
                list.retain(|(p, _)| p.clone().restore(&ctx).is_ok_and(|p| p != promise));
            } else {
                list.push((
                    Persistent::save(&ctx, promise),
                    Persistent::save(&ctx, reason),
                ));
            }
        }
    })));
    let Ok(context) = Context::full(&runtime) else {
        return End::stopped("the JavaScript engine could not start");
    };
    let shared = Rc::new(Shared {
        to_host,
        clock,
        outstanding: RefCell::default(),
        next_id: Cell::new(0),
        gone: Cell::new(false),
    });
    let end = context.with(|ctx| {
        let end = Session {
            ctx: ctx.clone(),
            shared: shared.clone(),
            rejections: rejections.clone(),
        }
        .run(job, from_host);
        // Nothing of the runtime may outlive it.
        rejections.borrow_mut().clear();
        end
    });
    runtime.set_host_promise_rejection_tracker(None);
    runtime.set_interrupt_handler(None);
    end
}

/// The prelude's functions the engine calls.
struct Api<'js> {
    start: Function<'js>,
    settle: Function<'js>,
    render: Function<'js>,
    format_error: Function<'js>,
    reason: Function<'js>,
    save_state: Function<'js>,
}

struct Session<'js> {
    ctx: Ctx<'js>,
    shared: Rc<Shared>,
    rejections: Rejections,
}

impl<'js> Session<'js> {
    fn run(self, job: Job, from_host: &Receiver<ToEngine>) -> End {
        let api = match self.setup(&job) {
            Ok(api) => api,
            Err(error) => {
                let why = self.plain_error(error);
                return End::stopped(format!("the JavaScript engine could not start: {why}"));
            }
        };
        self.shared.clock.restart();
        let started = match compile(&self.ctx, &job.code) {
            Ok(script) => api.start.call::<_, Promise>((script,)),
            Err(error) => Err(error),
        };
        let mut notes = Vec::new();
        let mut never = false;
        let head = match started {
            Err(error) => {
                if self.shared.clock.stopped() {
                    None
                } else {
                    Some(Head::Threw(self.error_text(&api, error)))
                }
            }
            Ok(promise) => {
                never = self.drive(&api, &promise, from_host);
                if self.shared.clock.stopped() || never || self.shared.gone.get() {
                    None
                } else {
                    Some(self.outcome(&api, &promise))
                }
            }
        };
        let head = head.unwrap_or_else(|| {
            Head::Stopped(match self.shared.clock.stop.get() {
                Some(Stop::Cpu) => CPU_MESSAGE.into(),
                _ if never => NEVER_MESSAGE.into(),
                _ => "the script was stopped".into(),
            })
        });
        let (head, images) = match head {
            Head::Value(rendered) => {
                // `{text, images}` from the prelude's render.
                let rendered: Json = serde_json::from_str(&rendered).unwrap_or_default();
                let text = rendered["text"].as_str().unwrap_or("undefined").to_owned();
                let images = rendered["images"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_u64)
                    .map(|i| i as usize)
                    .collect();
                (Head::Value(text), images)
            }
            other => (other, Vec::new()),
        };
        self.report_rejections(&api);
        let state = self.save_state(&api, &mut notes);
        End {
            head,
            images,
            state,
            notes,
        }
    }

    fn setup(&self, job: &Job) -> rquickjs::Result<Api<'js>> {
        let ctx = &self.ctx;
        let natives = Object::new(ctx.clone())?;
        let shared = &self.shared;

        natives.set(
            "call",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |ctx: Ctx<'js>, name: String, args: String| -> rquickjs::Result<Value<'js>> {
                    let args: Json = serde_json::from_str(&args).unwrap_or(Json::Null);
                    match mapping::command(&name, &args) {
                        Ok((tab, command)) => {
                            let id = shared.start(|id| ToHost::Call { id, tab, command });
                            (id as f64).into_js(&ctx)
                        }
                        Err(message) => message.into_js(&ctx),
                    }
                }
            })?,
        )?;
        natives.set(
            "sleep",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |ms: f64| -> f64 { shared.start(|id| ToHost::Sleep { id, ms }) as f64 }
            })?,
        )?;
        natives.set(
            "cancel",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |id: f64| {
                    shared.outstanding.borrow_mut().remove(&(id as u64));
                }
            })?,
        )?;
        natives.set(
            "info",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |tab: String| -> Option<String> {
                    let info = shared.ask(|reply| ToHost::Info { tab, reply }).flatten()?;
                    Some(serde_json::json!({ "url": info.url, "title": info.title }).to_string())
                }
            })?,
        )?;
        natives.set(
            "picture",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |index: f64| -> Option<String> {
                    shared
                        .ask(|reply| ToHost::Picture {
                            index: index as usize,
                            reply,
                        })
                        .flatten()
                }
            })?,
        )?;
        natives.set(
            "log",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |level: String, text: String| shared.send(ToHost::Log { level, text })
            })?,
        )?;
        natives.set(
            "warn",
            Function::new(ctx.clone(), {
                let shared = shared.clone();
                move |text: String| shared.send(ToHost::Warn(text))
            })?,
        )?;

        let mut options = EvalOptions::default();
        options.global = true;
        options.strict = true;
        options.filename = Some("loft.js".into());
        let factory: Function = ctx.eval_with_options(PRELUDE, options)?;
        let api: Object = factory.call((natives, job.tab.as_str(), job.state.as_str()))?;
        Ok(Api {
            start: api.get("start")?,
            settle: api.get("settle")?,
            render: api.get("render")?,
            format_error: api.get("formatError")?,
            reason: api.get("reason")?,
            save_state: api.get("saveState")?,
        })
    }

    /// Run jobs and hand over what settles until the script's promise
    /// settles or the run is stopped. `true`: it can never settle.
    fn drive(
        &self,
        api: &Api<'js>,
        promise: &Promise<'js>,
        from_host: &Receiver<ToEngine>,
    ) -> bool {
        let clock = &self.shared.clock;
        loop {
            while !clock.stopped() && self.ctx.execute_pending_job() {}
            if clock.stopped() || self.shared.gone.get() {
                return false;
            }
            self.report_rejections(api);
            if promise.state() != PromiseState::Pending {
                return false;
            }
            if self.shared.outstanding.borrow().is_empty() {
                return true;
            }
            match from_host.recv() {
                Err(_) => {
                    self.shared.gone.set(true);
                    return false;
                }
                Ok(ToEngine::Stop) => {
                    clock.stop.set(Some(Stop::Cancelled));
                    return false;
                }
                Ok(ToEngine::Settled { id, result }) => {
                    if !self.shared.outstanding.borrow_mut().remove(&id) {
                        continue;
                    }
                    clock.restart();
                    let (ok, payload) = match result {
                        Ok(value) => (true, value.to_string()),
                        Err(message) => (false, message),
                    };
                    if api.settle.call::<_, ()>((id as f64, ok, payload)).is_err() {
                        let _ = self.ctx.catch();
                    }
                }
            }
        }
    }

    /// The settled script's value or error, as text.
    fn outcome(&self, api: &Api<'js>, promise: &Promise<'js>) -> Head {
        self.shared.clock.restart();
        match promise.result::<Value>() {
            Some(Ok(value)) => match api.render.call::<_, String>((value,)) {
                Ok(rendered) => Head::Value(rendered),
                Err(error) => {
                    let why = self.error_text(api, error);
                    Head::Threw(format!("Error: the result could not be shown: {why}"))
                }
            },
            Some(Err(error)) => Head::Threw(self.error_text(api, error)),
            None => Head::Stopped("the script ended without a result".into()),
        }
    }

    /// An exception as the agent reads it (name, message, the script's
    /// lines).
    fn error_text(&self, api: &Api<'js>, error: rquickjs::Error) -> String {
        if !matches!(error, rquickjs::Error::Exception) {
            return format!("Error: {error}");
        }
        let thrown = self.ctx.catch();
        if thrown.is_null() {
            return OUT_OF_MEMORY.into();
        }
        self.shared.clock.restart();
        match api.format_error.call::<_, String>((thrown.clone(),)) {
            Ok(text) => text,
            Err(_) => {
                let _ = self.ctx.catch();
                self.plain_value(&thrown)
            }
        }
    }

    /// An error without the prelude's help.
    fn plain_error(&self, error: rquickjs::Error) -> String {
        if matches!(error, rquickjs::Error::Exception) {
            let thrown = self.ctx.catch();
            return self.plain_value(&thrown);
        }
        error.to_string()
    }

    fn plain_value(&self, value: &Value<'js>) -> String {
        if value.is_null() {
            return OUT_OF_MEMORY.into();
        }
        if let Some(object) = value.as_object() {
            let name: Option<String> = object.get("name").ok();
            let message: Option<String> = object.get("message").ok();
            if let Some(message) = message {
                return format!("{}: {message}", name.unwrap_or_else(|| "Error".into()));
            }
        }
        match value.as_string().and_then(|s| s.to_string().ok()) {
            Some(text) => format!("Thrown: {text}"),
            None => format!("Thrown: a {}", value.type_name()),
        }
    }

    /// Rejections nobody handled since the last look become warnings.
    fn report_rejections(&self, api: &Api<'js>) {
        let pending: Vec<_> = self.rejections.borrow_mut().drain(..).collect();
        for (_, reason) in pending {
            let Ok(reason) = reason.restore(&self.ctx) else {
                continue;
            };
            let line = match api.reason.call::<_, String>((reason.clone(),)) {
                Ok(line) => line,
                Err(_) => {
                    let _ = self.ctx.catch();
                    self.plain_value(&reason)
                }
            };
            self.shared.send(ToHost::Warn(format!(
                "Unhandled promise rejection: {line} (await or catch every promise the script starts)"
            )));
        }
    }

    /// `state` as JSON, with a fresh CPU slice of its own whatever stopped
    /// the run.
    fn save_state(&self, api: &Api<'js>, notes: &mut Vec<String>) -> Option<String> {
        let clock = &self.shared.clock;
        clock.saving.set(true);
        clock.restart();
        let saved = api.save_state.call::<_, Option<String>>(());
        clock.saving.set(false);
        match saved {
            Ok(Some(json)) if json.len() > crate::MAX_STATE => {
                notes.push(format!(
                    "state is {} KB of JSON, more than the {} KB kept between runs: this run's changes to it were dropped",
                    json.len().div_ceil(1024),
                    crate::MAX_STATE / 1024
                ));
                None
            }
            Ok(Some(json)) => Some(json),
            Ok(None) => {
                notes.push("state could not be saved: it is not JSON".into());
                None
            }
            Err(error) => {
                let why = match error {
                    rquickjs::Error::Exception => {
                        let thrown = self.ctx.catch();
                        self.plain_value(&thrown)
                    }
                    other => other.to_string(),
                };
                notes.push(format!("state could not be saved: {why}"));
                None
            }
        }
    }
}

/// The script as an async function of its globals, in `script.js` (its
/// first line is line 1). A single expression is returned; anything else
/// runs as statements.
fn compile<'js>(ctx: &Ctx<'js>, code: &str) -> rquickjs::Result<Function<'js>> {
    let options = || {
        let mut options = EvalOptions::default();
        options.global = true;
        options.strict = true;
        options.filename = Some("script.js".into());
        options
    };
    let expression = code.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    if !expression.trim().is_empty() {
        let source = format!("(async function ({PARAMS}) {{ return ({expression}\n) }})");
        match ctx.eval_with_options::<Function, _>(source, options()) {
            Ok(function) => return Ok(function),
            Err(_) => {
                let _ = ctx.catch();
            }
        }
    }
    let source = format!("(async function ({PARAMS}) {{{code}\n}})");
    ctx.eval_with_options(source, options())
}

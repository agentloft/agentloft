//! `agentloftd simulator`: one running simulator's screen and input, over
//! stdin and stdout ([`crate::wire`]).
//!
//! Five threads: the main one runs the run loop CoreSimulator's callbacks
//! may need and checks how the screen is turned and whether the device is
//! still running; one reads requests; one carries out input, in order; one
//! takes pictures (at most 30 a second, only while someone
//! watches, and only when the screen changed); one reads what accessibility
//! sees, while its frames are outlined ([`ax`]).

#![allow(unsafe_code)] // Objective-C and C calls into Apple's frameworks; each has a SAFETY note.

mod ax;
mod picture;
mod sim;

use crate::geometry::{Held, Turn, picture_size};
use crate::keys;
use crate::wire::{self, Command, Event, Request, ScreenSize};
use anyhow::{Context, Result, bail};
use loft_protocol::{DeviceButton, DeviceInput, TouchPhase, TouchPoint};
use sim::{DOWN, Device, Shared, UP};
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Pictures are at least this far apart: at most every other frame of a
/// screen that redraws 60 times a second, 30 a second.
const MIN_GAP: Duration = Duration::from_millis(30);
/// The longest side of a picture, in pixels.
const LONGEST: u32 = 2400;
const JPEG_QUALITY: f64 = 0.7;
/// SimulatorKit drops a drag sent sooner than this after the last message.
const DRAG_GAP: Duration = Duration::from_millis(17);

pub struct Options {
    pub udid: String,
    /// A device set other than the user's (tests).
    pub set: Option<PathBuf>,
    /// The Xcode whose SimulatorKit to use (`xcode-select -p`).
    pub developer_dir: PathBuf,
}

struct State {
    out: Mutex<std::io::Stdout>,
    flags: Mutex<Flags>,
    wake: Condvar,
    quit: AtomicBool,
    turn: Mutex<Turn>,
    /// Counts the screen's changes (for the accessibility thread).
    changes: AtomicU64,
    /// The accessibility frames outlined, in points of the screen as the
    /// interface shows it (empty while off).
    outlines: Mutex<Vec<ax::Frame>>,
}

#[derive(Default)]
struct Flags {
    watching: bool,
    dirty: bool,
    surfaces_changed: bool,
    captures: Vec<u64>,
}

impl State {
    fn send(&self, kind: u8, parts: &[&[u8]]) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if wire::write_message(&mut *out, kind, parts).is_err() {
            self.stop();
        }
    }

    fn event(&self, event: &Event) {
        if let Ok(json) = serde_json::to_vec(event) {
            self.send(wire::KIND_EVENT, &[&json]);
        }
    }

    fn done(&self, id: u64, result: Result<()>) {
        self.event(&Event::Done {
            id,
            error: result.err().map(|e| format!("{e:#}")),
        });
    }

    fn change(&self, f: impl FnOnce(&mut Flags)) {
        let mut flags = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut flags);
        self.wake.notify_all();
    }

    fn stop(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.change(|_| {});
    }

    fn stopped(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }

    fn turn(&self) -> Turn {
        *self.turn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Run the helper until stdin ends or the device stops.
pub fn run(options: Options) -> Result<()> {
    let state = Arc::new(State {
        out: Mutex::new(std::io::stdout()),
        flags: Mutex::default(),
        wake: Condvar::new(),
        quit: AtomicBool::new(false),
        turn: Mutex::default(),
        changes: AtomicU64::new(0),
        outlines: Mutex::default(),
    });
    let opened = Device::open(
        &options.udid,
        options.set.as_deref(),
        &options.developer_dir,
    )
    .and_then(|device| {
        let surface = device.surface()?;
        let turn = Turn::from_interface(device.interface_orientation().unwrap_or(1));
        Ok((Arc::new(Shared(device)), surface, turn))
    });
    let (device, surface, turn) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            state.event(&Event::Ended {
                reason: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    let (width, height) = picture::surface_size(surface_ptr(&surface));
    let native = (width as u32, height as u32);
    let scale = device.0.scale;
    let screen = |turn: Turn| {
        let (width, height) = turn.shown_size(native.0, native.1);
        ScreenSize {
            width,
            height,
            scale,
            orientation: Some(turn.orientation()),
        }
    };
    *state.turn.lock().unwrap_or_else(|e| e.into_inner()) = turn;

    let on_change = {
        let state = state.clone();
        move || {
            state.changes.fetch_add(1, Ordering::Relaxed);
            state.change(|f| f.dirty = true)
        }
    };
    let on_surfaces = {
        let state = state.clone();
        move || {
            state.change(|f| {
                f.surfaces_changed = true;
                f.dirty = true;
            })
        }
    };
    let watched = match device.0.watch(on_change, on_surfaces) {
        Ok(watched) => watched,
        Err(e) => {
            state.event(&Event::Ended {
                reason: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    state.event(&Event::Ready {
        screen: screen(turn),
    });

    let (inputs, input_rx) = mpsc::channel::<Request>();
    let (outlining, outlining_rx) = mpsc::channel::<Request>();
    let pictures = spawn("pictures", {
        let (state, device) = (state.clone(), device.clone());
        move || pictures(&state, &device, surface, scale)
    });
    // The input and requests threads may be waiting on stdin or on the
    // device when the helper stops: they end with the process.
    let _ = [
        spawn("input", {
            let (state, device) = (state.clone(), device.clone());
            let held = Held::from_turn(turn);
            let set = options.set.clone();
            let udid = options.udid.clone();
            move || input(&state, &device, input_rx, held, Pasteboard { udid, set })
        }),
        spawn("requests", {
            let state = state.clone();
            move || requests(&state, inputs, outlining)
        }),
        spawn("accessibility", {
            let (state, device) = (state.clone(), device.clone());
            // The screen's area in points, to leave out the app's window.
            let area = native.0 as f64 * native.1 as f64 / (scale as f64 * scale as f64);
            move || accessibility(&state, &device, outlining_rx, area)
        }),
    ];

    // The main thread: CoreSimulator's callbacks, the screen's turn and the
    // device's state.
    let mut checked_state = Instant::now();
    let mut checked_turn = Instant::now();
    while !state.stopped() {
        run_loop_for(Duration::from_millis(50));
        if checked_turn.elapsed() >= Duration::from_millis(250) {
            checked_turn = Instant::now();
            if let Ok(orientation) = device.0.interface_orientation() {
                let now = Turn::from_interface(orientation);
                let changed = {
                    let mut turn = state.turn.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::replace(&mut *turn, now) != now
                };
                if changed {
                    state.event(&Event::Size {
                        screen: screen(now),
                    });
                    state.change(|f| f.dirty = true);
                }
            }
        }
        if checked_state.elapsed() >= Duration::from_secs(1) {
            checked_state = Instant::now();
            match device.0.state() {
                Ok(sim::BOOTED) => {}
                Ok(_) => {
                    state.event(&Event::Ended {
                        reason: "The simulator was shut down.".into(),
                    });
                    state.stop();
                }
                Err(e) => {
                    state.event(&Event::Ended {
                        reason: format!("{e:#}"),
                    });
                    state.stop();
                }
            }
        }
    }
    device.0.stop_watching(&watched);
    let _ = pictures.join();
    Ok(())
}

fn surface_ptr(
    surface: &Shared<objc2::rc::Retained<objc2::runtime::AnyObject>>,
) -> *const std::ffi::c_void {
    objc2::rc::Retained::as_ptr(&surface.0).cast()
}

/// A thread for work someone is watching (user-interactive quality of
/// service).
fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // SAFETY: sets this thread's own QoS class.
            unsafe {
                libc::pthread_set_qos_class_self_np(
                    libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
                    0,
                );
            }
            f()
        })
        .expect("spawn a thread")
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopRunInMode(
        mode: *const std::ffi::c_void,
        seconds: f64,
        return_after_source: bool,
    ) -> i32;
    static kCFRunLoopDefaultMode: *const std::ffi::c_void;
}

/// Run this thread's run loop for a while (sleep if it has nothing to run).
fn run_loop_for(time: Duration) {
    let start = Instant::now();
    // SAFETY: the current thread's run loop, in its default mode.
    let result = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, time.as_secs_f64(), false) };
    // 1: finished at once, having no sources.
    if result == 1 {
        std::thread::sleep(time.saturating_sub(start.elapsed()));
    }
}

/// Read requests from stdin until it ends.
fn requests(state: &State, inputs: mpsc::Sender<Request>, outlining: mpsc::Sender<Request>) {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("simulator helper: bad request {line:?}: {e}");
                continue;
            }
        };
        match request.command {
            Command::Watch { on } => {
                state.change(|f| {
                    f.watching = on;
                    // A new viewer gets a picture at once.
                    f.dirty |= on;
                });
                state.done(request.id, Ok(()));
            }
            Command::Capture => state.change(|f| f.captures.push(request.id)),
            Command::Input { .. } | Command::Orientation { .. } => {
                if inputs.send(request).is_err() {
                    break;
                }
            }
            Command::AccessibilityFrames { .. } => {
                if outlining.send(request).is_err() {
                    break;
                }
            }
        }
    }
    state.stop();
}

/// Take pictures while someone watches and the screen changes, and captures
/// when asked.
fn pictures(
    state: &State,
    device: &Shared<Device>,
    mut surface: Shared<objc2::rc::Retained<objc2::runtime::AnyObject>>,
    scale: f32,
) {
    let mut last: Option<Instant> = None;
    loop {
        let (frame, captures, surfaces_changed) = {
            let mut flags = state.flags.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped() {
                    return;
                }
                if !flags.captures.is_empty() {
                    break;
                }
                let wait = if flags.dirty && flags.watching {
                    // Paced by the screen's own changes (60 a second while
                    // it animates): the first change at least `MIN_GAP`
                    // after the last picture is pictured at once. Sleeping
                    // until a deadline instead ran late (macOS stretches
                    // short sleeps by several milliseconds), 24 a second.
                    match last.map(|at| at.elapsed()) {
                        Some(since) if since < MIN_GAP => MIN_GAP - since,
                        _ => break,
                    }
                } else {
                    Duration::from_millis(500)
                };
                // Woken by the next change; else the last one is pictured
                // when it's due.
                flags = state
                    .wake
                    .wait_timeout(flags, wait)
                    .map(|(f, _)| f)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
            let frame = flags.dirty && flags.watching;
            if frame {
                flags.dirty = false;
            }
            (
                frame,
                std::mem::take(&mut flags.captures),
                std::mem::take(&mut flags.surfaces_changed),
            )
        };
        if surfaces_changed {
            match device.0.surface() {
                Ok(s) => surface = s,
                Err(e) => eprintln!("simulator helper: {e:#}"),
            }
        }
        let turn = state.turn();
        let ptr = surface_ptr(&surface);
        let (width, height) = picture::surface_size(ptr);
        let outlines = state
            .outlines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let outline = |p: &mut picture::Pixels| {
            if !outlines.is_empty() {
                // Points of the interface to pixels of the picture.
                let (shown, _) = turn.shown_size(width as u32, height as u32);
                let factor = p.width as f64 * scale as f64 / shown as f64;
                picture::outline(p, &outlines, factor);
            }
        };
        for id in captures {
            let png = picture::read(ptr, width, height, turn).and_then(|mut p| {
                outline(&mut p);
                picture::encode(&mut p, None)
            });
            match png {
                Ok(png) => state.send(wire::KIND_CAPTURE, &[&id.to_be_bytes(), &png]),
                Err(e) => state.done(id, Err(e)),
            }
        }
        if frame {
            last = Some(Instant::now());
            let (w, h) = picture_size(width as u32, height as u32, scale, LONGEST);
            let jpeg = picture::read(ptr, w as usize, h as usize, turn).and_then(|mut p| {
                outline(&mut p);
                let jpeg = picture::encode(&mut p, Some(JPEG_QUALITY))?;
                Ok((p.width as u32, p.height as u32, jpeg))
            });
            match jpeg {
                Ok((w, h, jpeg)) => state.send(
                    wire::KIND_FRAME,
                    &[&w.to_be_bytes(), &h.to_be_bytes(), &jpeg],
                ),
                Err(e) => eprintln!("simulator helper: {e:#}"),
            }
        }
    }
}

/// How often the outlines are read again while the screen keeps changing,
/// and while it doesn't (an app may change what accessibility sees without
/// redrawing).
const OUTLINES_EVERY: Duration = Duration::from_millis(400);
const OUTLINES_AT_LEAST: Duration = Duration::from_secs(3);

/// While accessibility frames are outlined, read them again after the
/// screen changes, and have them pictured.
fn accessibility(
    state: &State,
    device: &Shared<Device>,
    requests: mpsc::Receiver<Request>,
    area: f64,
) {
    let mut reader: Option<ax::Accessibility> = None;
    let mut on = false;
    let mut seen = u64::MAX;
    let mut read_at: Option<Instant> = None;
    loop {
        let request = if on {
            match requests.recv_timeout(OUTLINES_EVERY) {
                Ok(r) => Some(r),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match requests.recv() {
                Ok(r) => Some(r),
                Err(_) => return,
            }
        };
        if state.stopped() {
            return;
        }
        if let Some(request) = request {
            let Command::AccessibilityFrames { on: wanted } = request.command else {
                continue;
            };
            on = wanted;
            if !on {
                state
                    .outlines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                state.change(|f| f.dirty = true);
                state.done(request.id, Ok(()));
                continue;
            }
            if reader.is_none() {
                match ax::Accessibility::new(&device.0) {
                    Ok(r) => reader = Some(r),
                    Err(e) => {
                        on = false;
                        state.done(request.id, Err(e));
                        continue;
                    }
                }
            }
            // A device that just started has no app in front for a while.
            let mut result = read_outlines(state, reader.as_ref(), area);
            for _ in 0..10 {
                let empty = state
                    .outlines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty();
                if result.is_err() || !empty {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
                result = read_outlines(state, reader.as_ref(), area);
            }
            seen = state.changes.load(Ordering::Relaxed);
            read_at = Some(Instant::now());
            state.done(request.id, result);
            continue;
        }
        let changes = state.changes.load(Ordering::Relaxed);
        let stale = read_at.is_none_or(|at| at.elapsed() >= OUTLINES_AT_LEAST);
        if changes != seen || stale {
            seen = changes;
            read_at = Some(Instant::now());
            if let Err(e) = read_outlines(state, reader.as_ref(), area) {
                eprintln!("simulator helper: {e:#}");
            }
        }
    }
}

/// Read the frames and, if they changed, have the screen pictured again.
fn read_outlines(state: &State, reader: Option<&ax::Accessibility>, area: f64) -> Result<()> {
    let Some(reader) = reader else { return Ok(()) };
    let Some(mut frames) = reader.frames()? else {
        // Nothing in front to read yet: read again soon.
        return Ok(());
    };
    // Leave out what covers (nearly) the whole screen: the app's window.
    frames.retain(|f| f.width * f.height < 0.9 * area);
    let mut outlines = state.outlines.lock().unwrap_or_else(|e| e.into_inner());
    if *outlines != frames {
        *outlines = frames;
        drop(outlines);
        state.change(|f| f.dirty = true);
    }
    Ok(())
}

/// Text a US keyboard can't type goes through the device's pasteboard.
struct Pasteboard {
    udid: String,
    set: Option<PathBuf>,
}

impl Pasteboard {
    fn copy(&self, text: &str) -> Result<()> {
        let mut command = std::process::Command::new("xcrun");
        command.arg("simctl");
        if let Some(set) = &self.set {
            command.arg("--set").arg(set);
        }
        let mut child = command
            .args(["pbcopy", &self.udid])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("couldn't run simctl")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        if !child.wait()?.success() {
            bail!("simctl couldn't set the simulator's pasteboard");
        }
        Ok(())
    }
}

/// Where the fingers are on the screen as built.
type Fingers = Vec<(f64, f64)>;

/// Fingers of the upright picture, on the screen as built.
fn fingers_of(turn: Turn, touches: &[TouchPoint]) -> Result<Fingers> {
    if touches.is_empty() {
        bail!("a touch needs at least one finger");
    }
    if touches.len() > sim::MOST_FINGERS {
        bail!(
            "iOS simulators take at most {} fingers at once",
            sim::MOST_FINGERS
        );
    }
    Ok(touches
        .iter()
        .map(|p| turn.to_device(p.x as f64, p.y as f64))
        .collect())
}

/// Carry out input, in order.
fn input(
    state: &State,
    device: &Shared<Device>,
    requests: mpsc::Receiver<Request>,
    mut held: Held,
    pasteboard: Pasteboard,
) {
    let device = &device.0;
    let mut backlog: VecDeque<Request> = VecDeque::new();
    // Fingers are down; a drag SimulatorKit dropped for coming too soon.
    let mut touching = false;
    let mut pending: Option<Fingers> = None;
    loop {
        let request = match backlog.pop_front() {
            Some(r) => r,
            None if pending.is_some() => match requests.recv_timeout(DRAG_GAP) {
                Ok(r) => r,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(at) = &pending
                        && device.fingers(6, at).unwrap_or(true)
                    {
                        pending = None;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match requests.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        if state.stopped() {
            return;
        }
        let input = match request.command {
            Command::Input { input } => input,
            Command::Orientation { orientation } => {
                held = Held::of(orientation);
                state.done(request.id, device.hold(held.0));
                continue;
            }
            _ => continue,
        };
        let turn = state.turn();
        let mut fingers = |phase: TouchPhase, at: Fingers| match phase {
            TouchPhase::Down => {
                pending = None;
                touching = true;
                device.fingers(1, &at).map(drop)
            }
            TouchPhase::Move if touching => match device.fingers(6, &at) {
                Ok(true) => {
                    pending = None;
                    Ok(())
                }
                Ok(false) => {
                    pending = Some(at);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            TouchPhase::Move => Ok(()),
            TouchPhase::Up | TouchPhase::Cancel => {
                touching = false;
                if let Some(before) = pending.take() {
                    let _ = device.fingers(6, &before);
                }
                device.fingers(2, &at).map(drop)
            }
        };
        let result = match input {
            DeviceInput::Touch { phase, x, y } => {
                fingers(phase, vec![turn.to_device(x as f64, y as f64)])
            }
            DeviceInput::Touches { phase, touches } => {
                fingers_of(turn, &touches).and_then(|f| fingers(phase, f))
            }
            DeviceInput::Scroll { x, y, dx, dy } => {
                // Wheel events come in bursts: one drag for all that are
                // waiting at the same place.
                let (mut dx, mut dy) = (dx, dy);
                let mut ids = Vec::new();
                while let Ok(next) = requests.try_recv() {
                    match &next.command {
                        Command::Input {
                            input:
                                DeviceInput::Scroll {
                                    dx: more_x,
                                    dy: more_y,
                                    ..
                                },
                        } => {
                            dx += more_x;
                            dy += more_y;
                            ids.push(next.id);
                        }
                        _ => {
                            backlog.push_back(next);
                            break;
                        }
                    }
                }
                let result = scroll(device, turn, x as f64, y as f64, dx as f64, dy as f64);
                for id in ids {
                    state.done(id, Ok(()));
                }
                result
            }
            DeviceInput::Text { text } => type_text(device, &text, &pasteboard),
            DeviceInput::Key { key, modifiers } => match keys::named(&key) {
                Some(stroke) => press(device, stroke, modifiers),
                None => Err(anyhow::anyhow!(
                    "no key {key:?} on the simulator's keyboard"
                )),
            },
            DeviceInput::Button { button } => match button {
                DeviceButton::Home => tap_button(device, 0),
                DeviceButton::Power => tap_button(device, 1),
                DeviceButton::Recents => tap_button(device, 0).and_then(|()| {
                    std::thread::sleep(Duration::from_millis(120));
                    tap_button(device, 0)
                }),
                DeviceButton::VolumeUp => tap_usage(device, 0x0c, 0xe9),
                DeviceButton::VolumeDown => tap_usage(device, 0x0c, 0xea),
                DeviceButton::RotateLeft => {
                    held = held.left();
                    device.hold(held.0)
                }
                DeviceButton::RotateRight => {
                    held = held.right();
                    device.hold(held.0)
                }
                DeviceButton::Back => Err(anyhow::anyhow!("iOS has no Back button.")),
            },
        };
        state.done(request.id, result);
    }
}

fn tap_button(device: &Device, source: u32) -> Result<()> {
    device.button(source, DOWN)?;
    std::thread::sleep(Duration::from_millis(50));
    device.button(source, UP)
}

fn tap_usage(device: &Device, page: u32, usage: u32) -> Result<()> {
    device.usage(page, usage, DOWN)?;
    std::thread::sleep(Duration::from_millis(50));
    device.usage(page, usage, UP)
}

/// A short drag that moves the content by `dx`, `dy` (fractions of the
/// picture; positive shows what is further down or right), held still at
/// the end so it doesn't fling.
fn scroll(device: &Device, turn: Turn, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
    const STEPS: u32 = 6;
    let end = ((x - dx).clamp(0.02, 0.98), (y - dy).clamp(0.02, 0.98));
    let at = |t: f64| turn.to_device(x + (end.0 - x) * t, y + (end.1 - y) * t);
    let (sx, sy) = at(0.0);
    device.touch(1, sx, sy)?;
    for step in 1..=STEPS {
        std::thread::sleep(DRAG_GAP);
        let (px, py) = at(step as f64 / STEPS as f64);
        device.touch(6, px, py)?;
    }
    std::thread::sleep(Duration::from_millis(80));
    let (ex, ey) = at(1.0);
    device.touch(6, ex, ey)?;
    device.touch(2, ex, ey).map(drop)
}

fn press(device: &Device, stroke: keys::Stroke, modifiers: u8) -> Result<()> {
    let mut held: Vec<u32> = keys::MODIFIERS
        .iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, usage)| *usage)
        .collect();
    if stroke.shift && !held.contains(&keys::LEFT_SHIFT) {
        held.push(keys::LEFT_SHIFT);
    }
    for usage in &held {
        device.key(*usage, DOWN)?;
    }
    let result = device
        .key(stroke.usage, DOWN)
        .and_then(|()| device.key(stroke.usage, UP));
    for usage in held.iter().rev() {
        let _ = device.key(*usage, UP);
    }
    result
}

fn type_text(device: &Device, text: &str, pasteboard: &Pasteboard) -> Result<()> {
    let strokes: Option<Vec<keys::Stroke>> = text.chars().map(keys::stroke).collect();
    match strokes {
        Some(strokes) => {
            for stroke in strokes {
                press(device, stroke, 0)?;
            }
            Ok(())
        }
        None => {
            pasteboard.copy(text)?;
            press(
                device,
                keys::Stroke {
                    usage: keys::V,
                    shift: false,
                },
                loft_protocol::page_modifiers::META,
            )
        }
    }
}

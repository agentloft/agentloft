//! An agent's mouse and keyboard: real input over DevTools
//! (`Input.dispatchMouseEvent`, `dispatchKeyEvent`, `insertText`), which
//! pages see as trusted, human-like in its timing when `[browser]
//! human_input` is on (`human.rs`).
//!
//! One [`Input`] holds the page's input state for its whole life, so the
//! input of two commands never interleaves, and sends every event under the
//! page's `shaping` lock, so none lands while the page is pictured at
//! another scale.

use super::automation::{Agent, In};
use super::human::{self, InputState, Point};
use super::keys;
use loft_page_script::{BrowserError, SCRIPT_TIMEOUT};
use loft_protocol::page_modifiers::{ALT, CONTROL, META, SHIFT};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::MutexGuard;

/// The longest [`Input::settle`] waits for a frame that doesn't come.
const SETTLE_LATE: Duration = Duration::from_millis(100);
/// How long [`Input::settle`] waits for the page to answer at all.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

/// A key as the protocol wants it (US layout).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyDef {
    pub key: String,
    pub code: String,
    /// What it types; empty for none.
    pub text: String,
    /// 1 for the left modifier keys, else 0.
    pub location: u8,
    /// Its bit in `modifiers` when it is a modifier key, else 0.
    pub modifier: u8,
}

impl KeyDef {
    fn new(key: &str, code: &str, text: &str) -> Self {
        Self {
            key: key.to_owned(),
            code: code.to_owned(),
            text: text.to_owned(),
            location: 0,
            modifier: 0,
        }
    }

    fn modifier(key: &str, bit: u8) -> Self {
        Self {
            key: key.to_owned(),
            code: format!("{key}Left"),
            text: String::new(),
            location: 1,
            modifier: bit,
        }
    }
}

const MODIFIER_NAMES: &str = "Shift, Control, Alt, Meta or ControlOrMeta";

/// The modifier key a name stands for (case-insensitive, trimmed).
fn modifier_key(name: &str) -> Option<KeyDef> {
    let control_or_meta = if cfg!(target_os = "macos") {
        ("Meta", META)
    } else {
        ("Control", CONTROL)
    };
    let (key, bit) = match name.trim().to_ascii_lowercase().as_str() {
        "shift" => ("Shift", SHIFT),
        "control" | "ctrl" => ("Control", CONTROL),
        "alt" | "option" | "opt" => ("Alt", ALT),
        "meta" | "cmd" | "command" | "super" => ("Meta", META),
        "controlormeta" | "cmdorctrl" | "commandorcontrol" => control_or_meta,
        _ => return None,
    };
    Some(KeyDef::modifier(key, bit))
}

/// A key by name: the named keys, `F1`–`F24`, `KeyA`–`KeyZ`,
/// `Digit0`–`Digit9`, or a character the US keyboard types. With it,
/// whether the character needs Shift.
fn key_by_name(name: &str) -> Option<(KeyDef, bool)> {
    let named = |key: &str, text: &str| Some((KeyDef::new(key, key, text), false));
    match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => return named("Enter", "\r"),
        "tab" => return named("Tab", ""),
        "backspace" => return named("Backspace", ""),
        "escape" | "esc" => return named("Escape", ""),
        "delete" | "del" => return named("Delete", ""),
        "insert" => return named("Insert", ""),
        "home" => return named("Home", ""),
        "end" => return named("End", ""),
        "pageup" | "pgup" => return named("PageUp", ""),
        "pagedown" | "pgdn" => return named("PageDown", ""),
        "arrowleft" | "left" => return named("ArrowLeft", ""),
        "arrowup" | "up" => return named("ArrowUp", ""),
        "arrowright" | "right" => return named("ArrowRight", ""),
        "arrowdown" | "down" => return named("ArrowDown", ""),
        "space" | "spacebar" => return Some((KeyDef::new(" ", "Space", " "), false)),
        "capslock" => return named("CapsLock", ""),
        "contextmenu" => return named("ContextMenu", ""),
        _ => {}
    }
    if name.chars().count() > 1
        && let Some(key) = modifier_key(name)
    {
        return Some((key, false));
    }
    if let Some(n) = name.strip_prefix('F')
        && let Ok(n) = n.parse::<u32>()
        && (1..=24).contains(&n)
        && name == format!("F{n}")
    {
        return named(name, "");
    }
    if let Some(rest) = name
        .strip_prefix("Key")
        .or_else(|| name.strip_prefix("Digit"))
    {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let fits = if name.starts_with("Key") {
                c.is_ascii_uppercase()
            } else {
                c.is_ascii_digit()
            };
            if fits {
                return char_key(c.to_ascii_lowercase());
            }
        }
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => char_key(c),
        _ => None,
    }
}

/// The key a character is typed with, and whether it needs Shift. `None`
/// for what the US keyboard doesn't type (é, ß, 你, emoji).
fn char_key(c: char) -> Option<(KeyDef, bool)> {
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    const PUNCTUATION: [(char, char, &str); 11] = [
        ('`', '~', "Backquote"),
        ('-', '_', "Minus"),
        ('=', '+', "Equal"),
        ('[', '{', "BracketLeft"),
        (']', '}', "BracketRight"),
        ('\\', '|', "Backslash"),
        (';', ':', "Semicolon"),
        ('\'', '"', "Quote"),
        (',', '<', "Comma"),
        ('.', '>', "Period"),
        ('/', '?', "Slash"),
    ];
    let typed = |code: String, shift: bool| {
        let text = c.to_string();
        Some((KeyDef::new(&text, &code, &text), shift))
    };
    match c {
        'a'..='z' => typed(format!("Key{}", c.to_ascii_uppercase()), false),
        'A'..='Z' => typed(format!("Key{c}"), true),
        '0'..='9' => typed(format!("Digit{c}"), false),
        ' ' => Some((KeyDef::new(" ", "Space", " "), false)),
        '\n' | '\r' => Some((KeyDef::new("Enter", "Enter", "\r"), false)),
        '\t' => Some((KeyDef::new("Tab", "Tab", ""), false)),
        _ => {
            if let Some(digit) = SHIFTED_DIGITS.find(c) {
                return typed(format!("Digit{digit}"), true);
            }
            PUNCTUATION.iter().find_map(|&(plain, shifted, code)| {
                if c == plain {
                    typed(code.to_owned(), false)
                } else if c == shifted {
                    typed(code.to_owned(), true)
                } else {
                    None
                }
            })
        }
    }
}

/// A key combination: modifiers to hold, and the key (its name as given,
/// or a modifier's normalised).
#[derive(Debug, PartialEq)]
struct Combo {
    modifiers: Vec<KeyDef>,
    key: String,
}

/// `"Control+Shift+K"` → [Control, Shift] + `K`. A `+` that would start a
/// part is the `+` key (`Shift++`); a lone space is the Space key.
fn parse_combo(combo: &str) -> Result<Combo, BrowserError> {
    let trimmed = combo.trim();
    if trimmed.is_empty() {
        if combo.contains(' ') {
            return Ok(Combo {
                modifiers: Vec::new(),
                key: " ".into(),
            });
        }
        return Err(BrowserError::Action("no key given".into()));
    }
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut dangling = false;
    for c in trimmed.chars() {
        if c == '+' && !part.trim().is_empty() {
            parts.push(std::mem::take(&mut part));
            dangling = true;
        } else {
            part.push(c);
            dangling = false;
        }
    }
    if dangling || part.trim().is_empty() {
        return Err(BrowserError::Action(format!(
            "{combo:?} ends without a key after the last +"
        )));
    }
    parts.push(part);
    let key = parts.pop().unwrap_or_default();
    let key = match key.trim() {
        "" => key,
        trimmed => trimmed.to_owned(),
    };
    let modifiers = parts
        .iter()
        .map(|name| {
            modifier_key(name).ok_or_else(|| {
                BrowserError::Action(format!(
                    "unknown modifier {:?} in {combo:?}; use {MODIFIER_NAMES}",
                    name.trim()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let key = match modifier_key(&key) {
        Some(modifier) if key.chars().count() > 1 => modifier.key,
        _ => key,
    };
    if key_by_name(&key).is_none() && key.chars().count() != 1 {
        return Err(unknown_key(&key));
    }
    Ok(Combo { modifiers, key })
}

fn unknown_key(key: &str) -> BrowserError {
    BrowserError::Action(format!(
        "unknown key {key:?}: give a character, or a name such as Enter, Tab, Escape, \
         Backspace, ArrowDown, PageDown or F5"
    ))
}

fn button_bit(button: &str) -> u8 {
    match button {
        "right" => 2,
        "middle" => 4,
        _ => 1,
    }
}

fn button_name(bit: u8) -> &'static str {
    match bit {
        2 => "right",
        4 => "middle",
        _ => "left",
    }
}

/// The agent's mouse and keyboard on one page.
pub(super) struct Input<'a> {
    agent: &'a Agent<'a>,
    state: MutexGuard<'a, InputState>,
    /// Modifier keys held down by this `Input`, in the order pressed.
    keys: Vec<KeyDef>,
    /// Their bits.
    modifiers: u8,
    /// The viewport's size, once asked for (`None` inside: unknown).
    viewport: Option<Option<(f64, f64)>>,
}

impl<'a> Input<'a> {
    pub async fn new(agent: &'a Agent<'a>) -> Input<'a> {
        Input {
            agent,
            state: agent.driver.input.lock().await,
            keys: Vec::new(),
            modifiers: 0,
            viewport: None,
        }
    }

    pub fn human(&self) -> bool {
        self.agent.options.human_input
    }

    async fn send(&self, method: &str, params: Value, what: &str) -> Result<Value, BrowserError> {
        let _shaping = self.agent.page.shaping.lock().await;
        self.agent.send(method, params, SCRIPT_TIMEOUT, what).await
    }

    async fn viewport(&mut self) -> Option<(f64, f64)> {
        if let Some(known) = self.viewport {
            return known;
        }
        let metrics = self
            .agent
            .send(
                "Page.getLayoutMetrics",
                json!({}),
                SCRIPT_TIMEOUT,
                "measuring the page",
            )
            .await
            .ok();
        let size = metrics.and_then(|m| {
            let viewport = &m["cssLayoutViewport"];
            let (w, h) = (
                viewport["clientWidth"].as_f64()?,
                viewport["clientHeight"].as_f64()?,
            );
            (w > 0.0 && h > 0.0).then_some((w, h))
        });
        self.viewport = Some(size);
        size
    }

    /// Wait out `wait` less what has passed since `since`.
    async fn wait_since(since: Instant, wait: Duration) {
        let left = wait.saturating_sub(since.elapsed());
        if !left.is_zero() {
            tokio::time::sleep(left).await;
        }
    }

    async fn pause(&mut self, low: f64, high: f64) {
        if self.human() {
            let wait = self.state.rng.millis(low, high);
            tokio::time::sleep(wait).await;
        }
    }

    // -- the mouse ---------------------------------------------------------------

    fn held_button(&self) -> &'static str {
        let buttons = self.state.buttons;
        [1, 2, 4]
            .into_iter()
            .find(|bit| buttons & bit != 0)
            .map_or("none", button_name)
    }

    async fn mouse_moved(&mut self, at: Point) -> Result<(), BrowserError> {
        let params = json!({
            "type": "mouseMoved",
            "x": at.x,
            "y": at.y,
            "button": self.held_button(),
            "buttons": self.state.buttons,
            "modifiers": self.modifiers,
        });
        self.state.pointer = Some(at);
        self.send("Input.dispatchMouseEvent", params, "moving the pointer")
            .await
            .map(drop)
    }

    /// Move the pointer to `to`: along a person's path with human input
    /// (`steps` then doesn't count), else in `steps` even moves. Returns
    /// the point used.
    pub async fn move_to(&mut self, to: Point, steps: usize) -> Result<Point, BrowserError> {
        let from = self.state.pointer;
        if self.human() {
            let viewport = self.viewport().await;
            let aim = human::clamp(self.state.aim_at(to), viewport);
            let from = match from {
                Some(from) => from,
                None => self.state.start_near(aim, viewport),
            };
            let samples = human::path(&mut self.state.rng, from, aim, viewport);
            if samples.is_empty() {
                self.mouse_moved(aim).await?;
            }
            for (point, wait) in samples {
                let sent = Instant::now();
                self.mouse_moved(point).await?;
                Self::wait_since(sent, wait).await;
            }
            self.state.arrived = Some(Instant::now());
            return Ok(aim);
        }
        if let Some(from) = from
            && steps > 1
        {
            for i in 1..steps {
                let t = i as f64 / steps as f64;
                let p = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
                self.mouse_moved(p).await?;
            }
        }
        self.mouse_moved(to).await?;
        self.state.arrived = Some(Instant::now());
        Ok(to)
    }

    /// One move to `to` exactly, whatever human input would do.
    pub async fn nudge(&mut self, to: Point) -> Result<(), BrowserError> {
        self.mouse_moved(to).await
    }

    /// Press `button` where the pointer is.
    pub async fn button_down(&mut self, button: &str, count: u32) -> Result<(), BrowserError> {
        if self.human()
            && let Some(arrived) = self.state.arrived
        {
            // A moment to settle after arriving (none if that was long ago).
            let settle = self.state.rng.millis(45.0, 130.0);
            Self::wait_since(arrived, settle).await;
        }
        let at = self.state.pointer.unwrap_or(Point::new(0.0, 0.0));
        self.state.buttons |= button_bit(button);
        let params = json!({
            "type": "mousePressed",
            "x": at.x,
            "y": at.y,
            "button": button,
            "buttons": self.state.buttons,
            "clickCount": count.max(1),
            "modifiers": self.modifiers,
        });
        self.send(
            "Input.dispatchMouseEvent",
            params,
            "pressing the mouse button",
        )
        .await
        .map(drop)
    }

    /// Release `button` where the pointer is.
    pub async fn button_up(&mut self, button: &str, count: u32) -> Result<(), BrowserError> {
        let at = self.state.pointer.unwrap_or(Point::new(0.0, 0.0));
        self.state.buttons &= !button_bit(button);
        let params = json!({
            "type": "mouseReleased",
            "x": at.x,
            "y": at.y,
            "button": button,
            "buttons": self.state.buttons,
            "clickCount": count.max(1),
            "modifiers": self.modifiers,
        });
        self.send(
            "Input.dispatchMouseEvent",
            params,
            "releasing the mouse button",
        )
        .await
        .map(drop)
    }

    /// Move to `at` and click `count` times with `button`.
    pub async fn click(&mut self, at: Point, button: &str, count: u32) -> Result<(), BrowserError> {
        self.move_to(at, 1).await?;
        for n in 1..=count.max(1) {
            if n > 1 {
                self.pause(70.0, 140.0).await;
            }
            self.button_down(button, n).await?;
            self.pause(60.0, 140.0).await;
            self.button_up(button, n).await?;
        }
        Ok(())
    }

    /// The wheel over `at`: one event, or a glide with human input.
    pub async fn wheel(&mut self, at: Point, dx: f64, dy: f64) -> Result<(), BrowserError> {
        // Always moved there: a pointer kept from the previous document
        // isn't aimed at this one's scroller.
        let at = self.move_to(at, 1).await?;
        let events = if self.human() {
            human::glide(&mut self.state.rng, dx, dy)
        } else {
            vec![((dx, dy), Duration::ZERO)]
        };
        let round = |v: f64| (v * 100.0).round() / 100.0;
        for ((ex, ey), wait) in events {
            let sent = Instant::now();
            let params = json!({
                "type": "mouseWheel",
                "x": at.x,
                "y": at.y,
                "deltaX": round(ex),
                "deltaY": round(ey),
                "modifiers": self.modifiers,
            });
            self.send("Input.dispatchMouseEvent", params, "turning the wheel")
                .await?;
            Self::wait_since(sent, wait).await;
        }
        Ok(())
    }

    // -- HTML5 drag and drop ---------------------------------------------------------

    /// While on, Chrome hands an HTML5 drag over (`Input.dragIntercepted`)
    /// instead of running it (a headless browser can't finish one).
    pub async fn intercept_drags(&self, enabled: bool) {
        let _ = self
            .send(
                "Input.setInterceptDrags",
                json!({ "enabled": enabled }),
                "intercepting drags",
            )
            .await;
    }

    /// `dragEnter`, `dragOver` or `drop` at `at`, carrying `data`.
    pub async fn drag_event(
        &self,
        kind: &str,
        at: Point,
        data: &Value,
    ) -> Result<(), BrowserError> {
        let params = json!({
            "type": kind,
            "x": at.x,
            "y": at.y,
            "data": data,
            "modifiers": self.modifiers,
        });
        self.send("Input.dispatchDragEvent", params, "dropping")
            .await
            .map(drop)
    }

    // -- the keyboard ---------------------------------------------------------------

    /// The pause before a key goes down: `gap`, or a person's 30–110 ms
    /// (at most `cap`); none after a second without keys.
    async fn key_gap(&mut self, gap: Option<Duration>, cap: Option<Duration>) {
        if !self.human() {
            return;
        }
        let Some(last) = self.state.last_key else {
            return;
        };
        if last.elapsed() > Duration::from_secs(1) {
            return;
        }
        let mut wait = gap.unwrap_or_else(|| self.state.rng.millis(30.0, 110.0));
        if let Some(cap) = cap {
            wait = wait.min(cap);
        }
        Self::wait_since(last, wait).await;
    }

    async fn key_down(&mut self, key: &KeyDef) -> Result<(), BrowserError> {
        if key.modifier != 0 {
            self.modifiers |= key.modifier;
            self.keys.push(key.clone());
        }
        // A shortcut types nothing; Shift alone still does.
        let shortcut = self.modifiers & (CONTROL | ALT | META) != 0;
        let text = if shortcut { "" } else { key.text.as_str() };
        let vk = keys::virtual_key(&key.code, &key.key);
        let mut params = json!({
            "type": if text.is_empty() { "rawKeyDown" } else { "keyDown" },
            "modifiers": self.modifiers,
            "windowsVirtualKeyCode": vk,
            "nativeVirtualKeyCode": vk,
            "code": key.code,
            "key": key.key,
            "location": key.location,
        });
        if !text.is_empty() {
            params["text"] = json!(text);
            params["unmodifiedText"] = json!(text);
        }
        let command = keys::clipboard_command(&key.key, self.modifiers).or_else(|| {
            cfg!(target_os = "macos")
                .then(|| keys::mac_command(&key.key, self.modifiers))
                .flatten()
        });
        if let Some(command) = command {
            params["commands"] = json!([command]);
        }
        self.state.last_key = Some(Instant::now());
        self.send("Input.dispatchKeyEvent", params, "pressing a key")
            .await
            .map(drop)
    }

    async fn key_up(&mut self, key: &KeyDef) -> Result<(), BrowserError> {
        if key.modifier != 0 {
            self.modifiers &= !key.modifier;
            if let Some(i) = self.keys.iter().rposition(|k| k == key) {
                self.keys.remove(i);
            }
        }
        let vk = keys::virtual_key(&key.code, &key.key);
        let params = json!({
            "type": "keyUp",
            "modifiers": self.modifiers,
            "windowsVirtualKeyCode": vk,
            "nativeVirtualKeyCode": vk,
            "code": key.code,
            "key": key.key,
            "location": key.location,
        });
        self.send("Input.dispatchKeyEvent", params, "releasing a key")
            .await
            .map(drop)
    }

    /// Text put in at once, as a paste or an input method does (replacing
    /// any selection).
    pub async fn insert_text(&self, text: &str) -> Result<(), BrowserError> {
        self.send(
            "Input.insertText",
            json!({ "text": text }),
            "putting the text in",
        )
        .await
        .map(drop)
    }

    /// Wait until the page has dealt with the input just sent: its next
    /// frame, and the task after it (at most [`SETTLE_LATE`] when no frame
    /// comes). Editors that draw what was typed a frame later, then update
    /// their own state in a task of their own (React's effects, and most
    /// rich editors built on a framework) lost keys that came in between:
    /// the browser runs input before those tasks, and a later redraw put
    /// back an older state. A person is never that quick; this keeps an
    /// agent from being so. Errors don't count (a load in between), except
    /// a dialog the key opened.
    pub async fn settle(&self) -> Result<(), BrowserError> {
        let body = format!(
            "await new Promise((done) => {{\n  const late = setTimeout(done, {});\n  \
             requestAnimationFrame(() => setTimeout(() => {{ clearTimeout(late); done(); }}));\n}});",
            SETTLE_LATE.as_millis()
        );
        match self
            .agent
            .evaluate(&body, In::Helpers, SETTLE_TIMEOUT, "waiting for the page")
            .await
        {
            Err(err @ (BrowserError::DialogOpen(_) | BrowserError::Closed)) => Err(err),
            _ => Ok(()),
        }
    }

    /// Put `text` (and `html`) on the browser's clipboard: the daemon's
    /// headless browser has one of its own, in memory, so this computer's
    /// clipboard is never touched. Written from the helpers' world, so the
    /// page sees no script of ours (only, as with any copy, that the
    /// clipboard changed). Hold [`Browser::clipboard`] around it and the
    /// paste.
    async fn put_on_clipboard(&self, text: &str, html: Option<&str>) -> Result<(), BrowserError> {
        let text = serde_json::to_string(text).unwrap_or_default();
        let html = serde_json::to_string(&html).unwrap_or_default();
        let body = format!(
            "const items = {{ 'text/plain': new Blob([{text}], {{ type: 'text/plain' }}) }};\n\
             const html = {html};\n\
             if (html !== null) items['text/html'] = new Blob([html], {{ type: 'text/html' }});\n\
             await navigator.clipboard.write([new ClipboardItem(items)]);"
        );
        self.agent
            .evaluate(
                &body,
                In::Helpers,
                SCRIPT_TIMEOUT,
                "putting the text on the browser's clipboard",
            )
            .await
            .map(drop)
    }

    /// A real paste of `text` (and `html`, for editors that take
    /// formatting) where the focus is: the text goes on the browser's
    /// clipboard and the paste key (⌘V, Ctrl+V elsewhere) runs the
    /// browser's own paste command. The page gets one trusted `paste`
    /// event with both, and an editor (ProseMirror, Lexical, Draft, …)
    /// handles it as a person's paste: its own paragraphs and formatting,
    /// in one change. The clipboard is emptied again afterwards.
    ///
    /// `Ok(false)` when the browser's clipboard couldn't be written (the
    /// caller then puts the text in as typed input).
    pub async fn paste(&mut self, text: &str, html: Option<&str>) -> Result<bool, BrowserError> {
        let browser = self.agent.browser.clone();
        let _clipboard = browser.clipboard.lock().await;
        match self.put_on_clipboard(text, html).await {
            Ok(()) => {}
            Err(err @ (BrowserError::DialogOpen(_) | BrowserError::Closed)) => return Err(err),
            Err(err) => {
                tracing::debug!(%err, "the browser's clipboard couldn't be written");
                return Ok(false);
            }
        }
        let pasted = self.press("ControlOrMeta+v").await;
        // What was pasted isn't left for later pastes, nor for pages that
        // may read the clipboard.
        let _ = self.put_on_clipboard("", None).await;
        pasted?;
        Ok(true)
    }

    /// Text put in as an input method commits it (no key events), a line
    /// at a time, Enter between lines: a raw line break put into an
    /// editable element would stay a character in one paragraph instead
    /// of making a new one.
    pub async fn insert_lines(&mut self, text: &str) -> Result<(), BrowserError> {
        let text = text.replace("\r\n", "\n");
        for (n, line) in text.split(['\n', '\r']).enumerate() {
            if n > 0 {
                self.press("Enter").await?;
            }
            if !line.is_empty() {
                self.insert_text(line).await?;
                self.settle().await?;
            }
        }
        Ok(())
    }

    /// Hold modifier keys down; what to give [`Input::release`].
    pub async fn hold(&mut self, names: &[String]) -> Result<Vec<KeyDef>, BrowserError> {
        let keys = names
            .iter()
            .map(|name| {
                modifier_key(name).ok_or_else(|| {
                    BrowserError::Action(format!(
                        "unknown modifier {:?}; use {MODIFIER_NAMES}",
                        name.trim()
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        for key in &keys {
            self.key_gap(None, None).await;
            self.key_down(key).await?;
        }
        Ok(keys)
    }

    pub async fn release(&mut self, held: &[KeyDef]) -> Result<(), BrowserError> {
        for key in held.iter().rev() {
            self.key_up(key).await?;
        }
        Ok(())
    }

    /// A key or a combination (`"Control+Shift+K"`, `"ControlOrMeta+a"`).
    pub async fn press(&mut self, combo: &str) -> Result<(), BrowserError> {
        let Combo { modifiers, key } = parse_combo(combo)?;
        let Some((mut def, _)) = key_by_name(&key) else {
            // A character no key types: put in as text.
            self.key_gap(None, None).await;
            self.state.last_key = Some(Instant::now());
            self.insert_text(&key).await?;
            return self.settle().await;
        };
        for modifier in &modifiers {
            self.key_gap(None, None).await;
            self.key_down(modifier).await?;
        }
        // Shift makes a letter a capital, as a keyboard does.
        if self.modifiers & SHIFT != 0
            && def.key.len() == 1
            && def.key.as_bytes()[0].is_ascii_lowercase()
        {
            def.key = def.key.to_ascii_uppercase();
            def.text = def.text.to_ascii_uppercase();
        }
        self.key_gap(None, None).await;
        self.key_down(&def).await?;
        self.key_up(&def).await?;
        for modifier in modifiers.iter().rev() {
            self.key_up(modifier).await?;
        }
        self.settle().await
    }

    /// Text typed key by key (Shift where needed); what no key types is
    /// put in a character at a time. Each key waits for the page to have
    /// dealt with the one before ([`Input::settle`]); with human input,
    /// the pauses of a person's typing come on top, shortened so that
    /// they add at most about 3 s to the whole.
    pub async fn type_text(&mut self, text: &str) -> Result<(), BrowserError> {
        let count = text.chars().count().max(1);
        let cap = Duration::from_millis(3000) / count as u32;
        let mut previous = None;
        for c in text.chars() {
            // "\r\n" is one line break.
            if c == '\n' && previous == Some('\r') {
                previous = Some(c);
                continue;
            }
            previous = Some(c);
            match char_key(c) {
                Some((key, shift)) if shift && self.modifiers & SHIFT == 0 => {
                    let shift = KeyDef::modifier("Shift", SHIFT);
                    self.key_gap(None, Some(cap)).await;
                    self.key_down(&shift).await?;
                    self.key_down(&key).await?;
                    self.key_up(&key).await?;
                    self.key_up(&shift).await?;
                }
                Some((key, _)) => {
                    self.key_gap(None, Some(cap)).await;
                    self.key_down(&key).await?;
                    self.key_up(&key).await?;
                }
                None => {
                    self.key_gap(None, Some(cap)).await;
                    self.state.last_key = Some(Instant::now());
                    self.insert_text(&c.to_string()).await?;
                }
            }
            self.settle().await?;
        }
        Ok(())
    }

    /// Let go of every key and button still held (after a failure), errors
    /// ignored.
    pub async fn reset(&mut self) {
        while let Some(key) = self.keys.last().cloned() {
            // `key_up` lets go of it here even when the page doesn't hear.
            let _ = self.key_up(&key).await;
        }
        for bit in [1, 2, 4] {
            if self.state.buttons & bit != 0 {
                let _ = self.button_up(button_name(bit), 1).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(text: &str) -> Result<(Vec<String>, String), BrowserError> {
        parse_combo(text).map(|c| (c.modifiers.into_iter().map(|k| k.key).collect(), c.key))
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn combinations() {
        let meta_or_control = if cfg!(target_os = "macos") {
            "Meta"
        } else {
            "Control"
        };
        assert_eq!(
            combo("Control+Shift+K").unwrap(),
            (keys(&["Control", "Shift"]), "K".into())
        );
        assert_eq!(
            combo("ControlOrMeta+a").unwrap(),
            (keys(&[meta_or_control]), "a".into())
        );
        assert_eq!(
            combo("CmdOrCtrl+a").unwrap(),
            (keys(&[meta_or_control]), "a".into())
        );
        assert_eq!(
            combo("cmd+shift+z").unwrap(),
            (keys(&["Meta", "Shift"]), "z".into())
        );
        assert_eq!(combo("Enter").unwrap(), (keys(&[]), "Enter".into()));
        assert_eq!(combo("  Enter ").unwrap(), (keys(&[]), "Enter".into()));
        assert_eq!(combo("Shift++").unwrap(), (keys(&["Shift"]), "+".into()));
        assert_eq!(combo("+").unwrap(), (keys(&[]), "+".into()));
        assert_eq!(combo(" ").unwrap(), (keys(&[]), " ".into()));
        assert_eq!(
            combo("Alt+ctrl").unwrap(),
            (keys(&["Alt"]), "Control".into())
        );
        assert_eq!(
            combo("opt+ArrowLeft").unwrap(),
            (keys(&["Alt"]), "ArrowLeft".into())
        );
        let unknown = combo("Hyper+a").unwrap_err().to_string();
        assert!(
            unknown.contains("Hyper") && unknown.contains("ControlOrMeta"),
            "{unknown}"
        );
        let unknown = combo("NotAKey").unwrap_err().to_string();
        assert!(
            unknown.contains("Enter") && unknown.contains("F5"),
            "{unknown}"
        );
        assert!(combo("").unwrap_err().to_string().contains("no key"));
        assert!(combo("Shift+").is_err());
    }

    #[test]
    fn characters_and_names() {
        let (a, shift) = key_by_name("A").unwrap();
        assert_eq!(
            (a.code.as_str(), a.text.as_str(), shift),
            ("KeyA", "A", true)
        );
        let (q, shift) = key_by_name("?").unwrap();
        assert_eq!((q.code.as_str(), shift), ("Slash", true));
        let (at, shift) = char_key('@').unwrap();
        assert_eq!((at.code.as_str(), shift), ("Digit2", true));
        let (quote, shift) = char_key('\'').unwrap();
        assert_eq!((quote.code.as_str(), shift), ("Quote", false));
        assert!(char_key('é').is_none());
        assert!(key_by_name("é").is_none());
        let (f5, _) = key_by_name("F5").unwrap();
        assert_eq!((f5.key.as_str(), f5.code.as_str()), ("F5", "F5"));
        assert_eq!(keys::virtual_key(&f5.code, &f5.key), 116);
        assert!(key_by_name("F25").is_none());
        let (enter, _) = key_by_name("return").unwrap();
        assert_eq!((enter.key.as_str(), enter.text.as_str()), ("Enter", "\r"));
        assert_eq!(char_key('\n').unwrap().0.text, "\r");
        assert_eq!(key_by_name("esc").unwrap().0.key, "Escape");
        assert_eq!(key_by_name("ESCAPE").unwrap().0.key, "Escape");
        let (kq, shift) = key_by_name("KeyQ").unwrap();
        assert_eq!(
            (kq.key.as_str(), kq.code.as_str(), shift),
            ("q", "KeyQ", false)
        );
        assert_eq!(key_by_name("Digit7").unwrap().0.code, "Digit7");
        let (space, _) = key_by_name(" ").unwrap();
        assert_eq!((space.key.as_str(), space.code.as_str()), (" ", "Space"));
        let shift = key_by_name("shift").unwrap().0;
        assert_eq!(
            (shift.code.as_str(), shift.location, shift.modifier),
            ("ShiftLeft", 1, SHIFT)
        );
        assert_eq!(key_by_name("pgdn").unwrap().0.key, "PageDown");
        assert_eq!(key_by_name("Left").unwrap().0.key, "ArrowLeft");
        assert!(key_by_name("NotAKey").is_none());
    }
}

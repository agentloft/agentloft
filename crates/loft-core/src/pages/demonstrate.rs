//! `browser_demonstrate` (ADR-0017 §9): what a person shows an agent in a
//! tab, written down as steps the agent can repeat with its own tools.
//!
//! The steps come from the person's input as viewers send it (before it is
//! replayed into the page) and from the page's main-frame navigations. A
//! press is described by the element under it, typing by the field that has
//! focus. Of a password or one-time-code field only the number of characters
//! typed is kept.

use super::{Browser, Page, lock};
use loft_protocol::{PageButton, PageInput, PageMouse, page_modifiers};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// At most this many steps are kept.
const MAX_STEPS: usize = 400;
/// Text kept per typing step, in bytes; what comes after is only counted.
const MAX_TYPED: usize = 2000;
/// An element's name is cut off here (characters).
const MAX_NAME: usize = 80;
/// The isolated world elements are described in.
const WORLD: &str = "loft-demonstrate";

/// What a person showed in a tab.
#[derive(Default)]
pub(crate) struct Recording {
    stopped: AtomicBool,
    steps: Mutex<Vec<Step>>,
}

/// An element a step acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    tag: String,
    /// Empty for the page itself.
    selector: String,
    role: Option<String>,
    name: Option<String>,
    /// A password or one-time-code field.
    secret: bool,
}

impl Element {
    fn page() -> Self {
        Element {
            tag: "page".into(),
            selector: String::new(),
            role: None,
            name: None,
            secret: false,
        }
    }

    /// How a summary line names it.
    fn phrase(&self) -> String {
        if self.selector.is_empty() {
            return "the page".into();
        }
        let what = match (&self.role, &self.name) {
            (Some(role), Some(name)) => format!("the {role} \"{name}\""),
            (None, Some(name)) => format!("the {} \"{name}\"", self.tag),
            (Some(role), None) => format!("the {role}"),
            (None, None) => format!("the {}", self.tag),
        };
        format!("{what} ({})", self.selector)
    }

    /// The fields a structured step shares.
    fn fields(&self, step: &mut Value) {
        step["selector"] = json!(self.selector);
        if let Some(role) = &self.role {
            step["role"] = json!(role);
        }
        if let Some(name) = &self.name {
            step["name"] = json!(name);
        }
    }
}

#[derive(Debug, Clone)]
enum Step {
    Click {
        element: Element,
        count: u8,
        button: PageButton,
    },
    Type {
        element: Element,
        /// What is kept of the text (nothing for a secret field).
        text: String,
        /// Characters of `text` that are kept.
        kept: usize,
        /// Characters typed, kept or not.
        chars: usize,
    },
    Press {
        key: String,
    },
    Navigation {
        url: String,
    },
}

impl Step {
    fn typing(element: Element) -> Self {
        Step::Type {
            element,
            text: String::new(),
            kept: 0,
            chars: 0,
        }
    }

    /// Add typed text to a typing step.
    fn add_text(&mut self, typed: &str) {
        let Step::Type {
            element,
            text,
            kept,
            chars,
        } = self
        else {
            return;
        };
        for c in typed.chars() {
            *chars += 1;
            if !element.secret && *kept == *chars - 1 && text.len() + c.len_utf8() <= MAX_TYPED {
                text.push(c);
                *kept += 1;
            }
        }
    }

    /// Whether this is a typing step with something left to erase.
    fn erasable(&self) -> bool {
        matches!(self, Step::Type { chars, .. } if *chars > 0)
    }

    /// Take back the last character typed.
    fn erase(&mut self) {
        if let Step::Type {
            text, kept, chars, ..
        } = self
            && *chars > 0
        {
            if *kept == *chars {
                text.pop();
                *kept -= 1;
            }
            *chars -= 1;
        }
    }

    /// Nothing is left of it (all of its typing was erased).
    fn empty(&self) -> bool {
        matches!(self, Step::Type { chars: 0, .. })
    }

    fn to_json(&self) -> Value {
        match self {
            Step::Click {
                element,
                count,
                button,
            } => {
                let mut step = json!({ "kind": "click" });
                element.fields(&mut step);
                if *count > 1 {
                    step["count"] = json!(count);
                }
                if *button == PageButton::Right {
                    step["button"] = json!("right");
                }
                step
            }
            Step::Type {
                element,
                text,
                chars,
                ..
            } => {
                let mut step = json!({ "kind": "type" });
                element.fields(&mut step);
                if element.secret {
                    step["redacted"] = json!(true);
                    step["chars"] = json!(chars);
                } else {
                    step["text"] = json!(text);
                }
                step
            }
            Step::Press { key } => json!({ "kind": "press", "key": key }),
            Step::Navigation { url } => json!({ "kind": "navigation", "url": url }),
        }
    }

    fn line(&self) -> String {
        match self {
            Step::Click {
                element,
                count,
                button,
            } => {
                let verb = match (button, count) {
                    (PageButton::Right, _) => "Right-clicked",
                    (_, 2) => "Double-clicked",
                    (_, 3..) => "Triple-clicked",
                    _ => "Clicked",
                };
                format!("{verb} {}", element.phrase())
            }
            Step::Type { element, chars, .. } if element.secret => format!(
                "Typed a secret of {chars} characters (not recorded) into {}: ask the user to \
                 enter it",
                element.phrase()
            ),
            Step::Type { element, text, .. } => {
                format!("Typed {text:?} into {}", element.phrase())
            }
            Step::Press { key } => format!("Pressed {key}"),
            Step::Navigation { url } => format!("Went to {url}"),
        }
    }
}

/// What a key going down amounts to.
#[derive(Debug, PartialEq, Eq)]
enum KeyStep {
    /// A modifier on its own: nothing.
    Ignore,
    /// It types this.
    Type(String),
    /// Backspace with nothing held: erases the last character typed.
    Backspace,
    /// A key or shortcut, by name (`Enter`, `Meta+a`).
    Press(String),
}

fn key_step(key: &str, text: Option<&str>, modifiers: u8) -> KeyStep {
    if matches!(
        key,
        "" | "Shift" | "Control" | "Alt" | "Meta" | "CapsLock" | "Fn" | "OS" | "Unidentified"
    ) {
        return KeyStep::Ignore;
    }
    let held = |bit: u8| modifiers & bit != 0;
    let shortcut = held(page_modifiers::CONTROL) || held(page_modifiers::META);
    let special = matches!(key, "Enter" | "Tab" | "Escape" | "Backspace" | "Delete");
    if !shortcut
        && !special
        && let Some(text) = text.filter(|t| typed_text(t))
    {
        return KeyStep::Type(text.to_owned());
    }
    if key == "Backspace" && modifiers == 0 {
        return KeyStep::Backspace;
    }
    let mut name = String::new();
    for (bit, prefix) in [
        (page_modifiers::CONTROL, "Control"),
        (page_modifiers::ALT, "Alt"),
        (page_modifiers::META, "Meta"),
        (page_modifiers::SHIFT, "Shift"),
    ] {
        if held(bit) {
            name.push_str(prefix);
            name.push('+');
        }
    }
    name.push_str(if key == " " { "Space" } else { key });
    KeyStep::Press(name)
}

/// Text that is typing: something, and no control characters.
fn typed_text(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(char::is_control)
}

impl Recording {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Nothing more is written down.
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// The steps, for an agent to read as data.
    pub(crate) fn steps(&self) -> Vec<Value> {
        lock(&self.steps)
            .iter()
            .filter(|s| !s.empty())
            .map(Step::to_json)
            .collect()
    }

    /// One line per step, for an agent to read.
    pub(crate) fn summary(&self) -> Vec<String> {
        lock(&self.steps)
            .iter()
            .filter(|s| !s.empty())
            .map(Step::line)
            .collect()
    }

    /// Change the steps, unless recording stopped.
    fn edit<T>(&self, change: impl FnOnce(&mut Vec<Step>) -> T) -> Option<T> {
        if self.stopped.load(Ordering::Relaxed) {
            return None;
        }
        Some(change(&mut lock(&self.steps)))
    }

    fn push(&self, step: Step) {
        self.edit(|steps| {
            if steps.len() < MAX_STEPS {
                steps.push(step);
            }
        });
    }

    /// A press of `count` clicks makes the last click a double (or triple)
    /// click, if the last step is a click. Whether it did.
    fn more_clicks(&self, count: u8) -> bool {
        self.edit(|steps| match steps.last_mut() {
            Some(Step::Click { count: last, .. }) => {
                *last = count.min(3);
                true
            }
            _ => false,
        })
        .unwrap_or(true)
    }

    /// Typing that goes on in the last step, if that is a typing step.
    fn type_more(&self, text: &str) -> bool {
        self.edit(|steps| match steps.last_mut() {
            Some(step @ Step::Type { .. }) => {
                step.add_text(text);
                true
            }
            _ => false,
        })
        .unwrap_or(true)
    }

    fn type_into(&self, element: Element, text: &str) {
        let mut step = Step::typing(element);
        step.add_text(text);
        self.push(step);
    }

    /// Backspace: the last character typed is taken back. Whether there
    /// was one.
    fn erase(&self) -> bool {
        self.edit(|steps| match steps.last_mut() {
            Some(step) if step.erasable() => {
                step.erase();
                true
            }
            _ => false,
        })
        .unwrap_or(true)
    }

    fn navigated(&self, url: &str) {
        self.edit(|steps| {
            if matches!(steps.last(), Some(Step::Navigation { url: last }) if last == url) {
                return;
            }
            if steps.len() < MAX_STEPS {
                steps.push(Step::Navigation {
                    url: url.to_owned(),
                });
            }
        });
    }
}

impl Browser {
    /// A viewer's input to the tab, while it is being shown how.
    pub(super) async fn record_input(&self, page: &Page, input: &PageInput) {
        let Some(recording) = self.recording(&page.id) else {
            return;
        };
        match input {
            PageInput::Mouse {
                action: PageMouse::Down,
                x,
                y,
                button,
                click_count,
                ..
            } if *button != PageButton::None => {
                if *click_count >= 2 && recording.more_clicks(*click_count) {
                    return;
                }
                let element = self.describe(page, Some((*x, *y))).await;
                recording.push(Step::Click {
                    element,
                    count: (*click_count).max(1),
                    button: *button,
                });
            }
            PageInput::Key {
                down: true,
                key,
                text,
                modifiers,
                ..
            } => match key_step(key, text.as_deref(), *modifiers) {
                KeyStep::Ignore => {}
                KeyStep::Type(text) => self.record_typing(page, &recording, &text).await,
                KeyStep::Backspace => {
                    if !recording.erase() {
                        recording.push(Step::Press {
                            key: "Backspace".into(),
                        });
                    }
                }
                KeyStep::Press(key) => recording.push(Step::Press { key }),
            },
            PageInput::Text { text } => {
                let text: String = text.chars().filter(|c| !c.is_control()).collect();
                if !text.is_empty() {
                    self.record_typing(page, &recording, &text).await;
                }
            }
            _ => {}
        }
    }

    async fn record_typing(&self, page: &Page, recording: &Arc<Recording>, text: &str) {
        if recording.type_more(text) {
            return;
        }
        let element = self.describe(page, None).await;
        recording.type_into(element, text);
    }

    /// The tab's main frame went to `url`, while it is being shown how.
    pub(super) fn record_navigation(&self, page: &Page, url: &str) {
        let Some(recording) = self.recording(&page.id) else {
            return;
        };
        if url.is_empty() || url == "about:blank" || url.starts_with("chrome-error:") {
            return;
        }
        recording.navigated(url);
    }

    /// The element at a point (`Some`), or the one with focus (`None`).
    async fn describe(&self, page: &Page, at: Option<(f32, f32)>) -> Element {
        match self.try_describe(page, at).await {
            Ok(element) => element,
            Err(err) => {
                tracing::debug!(%err, "a demonstrated step's element wasn't described");
                Element::page()
            }
        }
    }

    async fn try_describe(&self, page: &Page, at: Option<(f32, f32)>) -> Result<Element, String> {
        let s = Some(page.session.as_str());
        let frame = lock(&page.state)
            .main_frame
            .clone()
            .ok_or("the page has no main frame yet")?;
        let world = self
            .cdp
            .send(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": WORLD }),
                s,
            )
            .await?;
        let context = world["executionContextId"]
            .as_i64()
            .ok_or("no execution context")?;
        let point = match at {
            Some((x, y)) => json!({ "x": x, "y": y }),
            None => Value::Null,
        };
        let reply = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": format!("({DESCRIBE_JS})({point})"),
                    "contextId": context,
                    "returnByValue": true,
                }),
                s,
            )
            .await?;
        if let Some(error) = reply.get("exceptionDetails") {
            return Err(error.to_string());
        }
        element_of(&reply["result"]["value"]).ok_or_else(|| "a reply without an element".into())
    }
}

/// The description the page's script returned (`null`: the page itself).
fn element_of(value: &Value) -> Option<Element> {
    if value.is_null() {
        return Some(Element::page());
    }
    let tag = value["tag"].as_str()?.to_ascii_lowercase();
    let selector = value["selector"].as_str()?.to_owned();
    if tag.is_empty() || selector.is_empty() {
        return Some(Element::page());
    }
    let text = |key: &str| {
        value[key]
            .as_str()
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
    };
    Some(Element {
        tag,
        selector,
        role: text("role"),
        name: text("name").map(|n| n.chars().take(MAX_NAME).collect()),
        secret: value["secret"].as_bool().unwrap_or(false),
    })
}

/// Describes the element at `point` ({x, y}), or the focused one (`null`):
/// `null` for the page itself.
const DESCRIBE_JS: &str = r#"(point) => {
  const INTERACTIVE = 'button, a[href], input, select, textarea, summary, label, '
    + '[role=button], [role=link], [role=tab], [role=menuitem], [role=option], '
    + '[role=checkbox], [role=radio], [contenteditable]:not([contenteditable=false])';
  const up = (el) => el.parentElement || (el.parentNode && el.parentNode.host) || null;
  let el = null;
  if (point) {
    el = document.elementFromPoint(point.x, point.y);
    while (el && el.shadowRoot) {
      const inner = el.shadowRoot.elementFromPoint(point.x, point.y);
      if (!inner || inner === el) break;
      el = inner;
    }
    for (let at = el; at; at = up(at)) {
      if (at.matches && at.matches(INTERACTIVE)) { el = at; break; }
    }
  } else {
    el = document.activeElement;
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
  }
  if (!el || el === document.body || el === document.documentElement) return null;
  const tag = el.localName;
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim().slice(0, 80) || null;
  const quoted = (v) => '"' + v.replace(/["\\]/g, '\\$&') + '"';
  const selectorOf = (node) => {
    if (node.id) return '#' + CSS.escape(node.id);
    const name = node.localName;
    const root = node.getRootNode();
    for (const attr of ['data-testid', 'data-test', 'name', 'aria-label']) {
      const value = node.getAttribute(attr);
      if (!value) continue;
      const s = name + '[' + attr + '=' + quoted(value) + ']';
      try { if (root.querySelectorAll(s).length === 1) return s; } catch (_) {}
    }
    const parent = node.parentElement;
    if (!parent || parent === document.documentElement) return name;
    let part = name;
    const same = Array.from(parent.children).filter((c) => c.localName === name);
    if (same.length > 1) part += ':nth-of-type(' + (same.indexOf(node) + 1) + ')';
    return selectorOf(parent) + ' > ' + part;
  };
  const roleOf = () => {
    const explicit = (el.getAttribute('role') || '').trim().split(/\s+/)[0];
    if (explicit) return explicit;
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    if (tag === 'button' || tag === 'summary') return 'button';
    if (tag === 'a') return 'link';
    if (tag === 'input') {
      if (['button', 'submit', 'reset'].includes(type)) return 'button';
      if (type === 'checkbox' || type === 'radio') return type;
      return 'textbox';
    }
    if (tag === 'textarea' || el.isContentEditable) return 'textbox';
    if (tag === 'select') return 'combobox';
    return null;
  };
  const nameOf = () => {
    let name = clean(el.getAttribute('aria-label'));
    if (name) return name;
    const ids = (el.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean);
    if (ids.length) {
      const root = el.getRootNode();
      name = clean(ids.map((id) => {
        const target = (root.getElementById && root.getElementById(id)) || document.getElementById(id);
        return target ? target.textContent : '';
      }).join(' '));
      if (name) return name;
    }
    if (el.labels && el.labels.length) {
      name = clean(el.labels[0].textContent);
      if (name) return name;
    }
    for (const attr of ['placeholder', 'alt', 'title', 'name']) {
      name = clean(el.getAttribute(attr));
      if (name) return name;
    }
    if (tag === 'input' || tag === 'textarea') return null;
    return clean(el.innerText !== undefined ? el.innerText : el.textContent);
  };
  const secret = (tag === 'input' && (el.getAttribute('type') || '').toLowerCase() === 'password')
    || (el.getAttribute('autocomplete') || '').toLowerCase().includes('one-time-code');
  return { tag, selector: selectorOf(el), role: roleOf(), name: nameOf(), secret };
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use page_modifiers::{META, SHIFT};

    fn field(selector: &str, role: &str, name: &str, secret: bool) -> Element {
        Element {
            tag: "input".into(),
            selector: selector.into(),
            role: Some(role.into()),
            name: Some(name.into()),
            secret,
        }
    }

    /// What `record_input` does, without a page to describe elements in.
    fn key(recording: &Recording, key: &str, text: Option<&str>, modifiers: u8, focus: &Element) {
        match key_step(key, text, modifiers) {
            KeyStep::Ignore => {}
            KeyStep::Type(text) => {
                if !recording.type_more(&text) {
                    recording.type_into(focus.clone(), &text);
                }
            }
            KeyStep::Backspace => {
                if !recording.erase() {
                    recording.push(Step::Press {
                        key: "Backspace".into(),
                    });
                }
            }
            KeyStep::Press(key) => recording.push(Step::Press { key }),
        }
    }

    #[test]
    fn steps_are_written_down() {
        let r = Recording::new();
        let button = Element {
            tag: "button".into(),
            selector: "#login".into(),
            role: Some("button".into()),
            name: Some("Sign in".into()),
            secret: false,
        };
        let name = field("#name", "textbox", "Your name", false);
        let password = field("#pw", "textbox", "Password", true);
        r.push(Step::Click {
            element: button.clone(),
            count: 1,
            button: PageButton::Left,
        });
        for c in ["a", "b"] {
            key(&r, c, Some(c), 0, &name);
        }
        key(&r, "c", Some("c"), 0, &name);
        key(&r, "Backspace", None, 0, &name);
        key(&r, "d", Some("d"), 0, &name);
        key(&r, "Tab", None, 0, &name);
        for c in "hunter2".chars() {
            let c = c.to_string();
            key(&r, &c, Some(&c), 0, &password);
        }
        key(&r, "Enter", Some("\r"), 0, &password);
        r.navigated("https://example.com/home");
        r.navigated("https://example.com/home");

        let summary = r.summary();
        assert_eq!(summary.len(), 6, "{summary:#?}");
        assert_eq!(summary[0], r#"Clicked the button "Sign in" (#login)"#);
        assert_eq!(
            summary[1],
            r#"Typed "abd" into the textbox "Your name" (#name)"#
        );
        assert_eq!(summary[2], "Pressed Tab");
        assert!(
            summary[3].contains("secret of 7 characters"),
            "{}",
            summary[3]
        );
        assert!(summary[3].contains("ask the user"), "{}", summary[3]);
        assert!(summary[3].contains("(#pw)"), "{}", summary[3]);
        assert_eq!(summary[4], "Pressed Enter");
        assert_eq!(summary[5], "Went to https://example.com/home");

        let steps = r.steps();
        assert_eq!(steps.len(), 6);
        assert_eq!(
            steps[0],
            json!({ "kind": "click", "selector": "#login", "role": "button", "name": "Sign in" })
        );
        assert_eq!(steps[1]["text"], "abd");
        assert_eq!(steps[3]["redacted"], true);
        assert_eq!(steps[3]["chars"], 7);
        assert!(steps[3].get("text").is_none());
        assert_eq!(
            steps[5],
            json!({ "kind": "navigation", "url": "https://example.com/home" })
        );
        let all = serde_json::to_string(&steps).unwrap() + &summary.join("\n");
        assert!(!all.contains("hunter"), "{all}");

        r.stop();
        r.push(Step::Press {
            key: "Enter".into(),
        });
        key(&r, "x", Some("x"), 0, &name);
        r.navigated("https://example.com/elsewhere");
        assert_eq!(r.summary(), summary);
    }

    #[test]
    fn clicks_count_and_erased_typing_vanishes() {
        let r = Recording::new();
        let link = Element {
            tag: "a".into(),
            selector: "nav > a:nth-of-type(2)".into(),
            role: Some("link".into()),
            name: None,
            secret: false,
        };
        r.push(Step::Click {
            element: link.clone(),
            count: 1,
            button: PageButton::Left,
        });
        assert!(r.more_clicks(2));
        r.push(Step::Click {
            element: Element::page(),
            count: 1,
            button: PageButton::Right,
        });
        let name = field("#q", "textbox", "Search", false);
        key(&r, "x", Some("x"), 0, &name);
        key(&r, "Backspace", None, 0, &name);
        // Nothing left to erase: the key itself is the step.
        key(&r, "Backspace", None, 0, &name);
        assert_eq!(
            r.summary(),
            vec![
                "Double-clicked the link (nav > a:nth-of-type(2))",
                "Right-clicked the page",
                "Pressed Backspace",
            ]
        );
        let steps = r.steps();
        assert_eq!(steps[0]["count"], 2);
        assert_eq!(steps[1]["button"], "right");
        assert_eq!(steps[1]["selector"], "");

        // Long typing is counted, not all kept.
        let r = Recording::new();
        let long = "x".repeat(MAX_TYPED + 10);
        r.type_into(name.clone(), &long);
        assert_eq!(r.steps()[0]["text"].as_str().unwrap().len(), MAX_TYPED);
        // At most so many steps.
        let r = Recording::new();
        for _ in 0..MAX_STEPS + 5 {
            r.push(Step::Press { key: "Tab".into() });
        }
        assert_eq!(r.steps().len(), MAX_STEPS);
    }

    #[test]
    fn keys_are_typing_or_presses() {
        assert_eq!(key_step("a", Some("a"), 0), KeyStep::Type("a".into()));
        assert_eq!(key_step("A", Some("A"), SHIFT), KeyStep::Type("A".into()));
        assert_eq!(
            key_step("a", Some("a"), META),
            KeyStep::Press("Meta+a".into())
        );
        assert_eq!(
            key_step("Enter", Some("\r"), 0),
            KeyStep::Press("Enter".into())
        );
        assert_eq!(
            key_step("ArrowDown", None, 0),
            KeyStep::Press("ArrowDown".into())
        );
        assert_eq!(
            key_step("Tab", None, SHIFT),
            KeyStep::Press("Shift+Tab".into())
        );
        assert_eq!(
            key_step(
                " ",
                Some(" "),
                page_modifiers::CONTROL | page_modifiers::ALT
            ),
            KeyStep::Press("Control+Alt+Space".into())
        );
        assert_eq!(key_step(" ", Some(" "), 0), KeyStep::Type(" ".into()));
        assert_eq!(key_step("Backspace", None, 0), KeyStep::Backspace);
        assert_eq!(
            key_step("Backspace", None, page_modifiers::ALT),
            KeyStep::Press("Alt+Backspace".into())
        );
        for modifier in ["Shift", "Control", "Alt", "Meta", "CapsLock", ""] {
            assert_eq!(key_step(modifier, None, SHIFT), KeyStep::Ignore);
        }
    }

    #[test]
    fn descriptions_are_read() {
        assert_eq!(element_of(&Value::Null), Some(Element::page()));
        let el = element_of(&json!({
            "tag": "INPUT", "selector": "#name", "role": "textbox",
            "name": "  Your\n name ", "secret": false
        }))
        .unwrap();
        assert_eq!(el, field("#name", "textbox", "Your name", false));
        assert_eq!(el.phrase(), r#"the textbox "Your name" (#name)"#);
        let plain = element_of(&json!({ "tag": "div", "selector": "main > div", "name": "Hi" }));
        assert_eq!(plain.unwrap().phrase(), r#"the div "Hi" (main > div)"#);
        assert_eq!(element_of(&json!({ "nothing": 1 })), None);
    }
}

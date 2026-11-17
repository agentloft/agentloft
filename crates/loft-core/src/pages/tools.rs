//! What a browser tab's toolbar offers beyond going somewhere: zoom, other
//! screens, a picture for the composer, notes for the agent, and clearing
//! what sites stored.

use super::{Browser, Page, Pages, bad, internal, lock};
use crate::BusMsg;
use base64::Engine as _;
use loft_protocol::{ErrorBody, PageAction, PageDevice, PageId, PageNote, PageRect};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Annotation mode's script (it installs itself once per document).
const ANNOTATE_JS: &str = include_str!("annotate.js");
/// The world it runs in: the page's own scripts can't see it.
const NOTES_WORLD: &str = "agentloft-notes";
/// How often notes are collected while the mode is on.
const NOTES_EVERY: Duration = Duration::from_millis(400);
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 5.0;

impl Pages {
    fn tab(&self, id: &PageId) -> Result<(Arc<Browser>, Arc<Page>), ErrorBody> {
        let (browser, page) = self.page(id)?;
        if page.view.is_some() {
            return Err(bad("A view isn't a browser tab."));
        }
        Ok((browser, page))
    }

    pub async fn zoom(&self, id: &PageId, zoom: f32) -> Result<(), ErrorBody> {
        let (browser, page) = self.tab(id)?;
        let zoom = if zoom.is_finite() {
            zoom.clamp(MIN_ZOOM, MAX_ZOOM)
        } else {
            1.0
        };
        let changed = {
            let mut state = lock(&page.state);
            state.info.zoom = zoom;
            state.shape()
        };
        if changed {
            browser.reshape(&page).await?;
        } else {
            browser.announce(&page);
        }
        Ok(())
    }

    pub async fn emulate(&self, id: &PageId, device: Option<PageDevice>) -> Result<(), ErrorBody> {
        let (browser, page) = self.tab(id)?;
        browser.emulate_device(&page, device).await
    }
}

impl Browser {
    /// Give the page a screen of its own (responsive mode), or none: every
    /// viewer shows the page at that size. An agent's `emulate` comes here
    /// too.
    pub(super) async fn emulate_device(
        self: &Arc<Self>,
        page: &Arc<Page>,
        device: Option<PageDevice>,
    ) -> Result<(), ErrorBody> {
        let (browser, page) = (self, page);
        let device = device.map(|mut d| {
            d.width = d.width.clamp(super::MIN_SIDE, super::MAX_SIDE);
            d.height = d.height.clamp(super::MIN_SIDE, super::MAX_SIDE);
            d
        });
        let (agent_before, mobile_before) = {
            let state = lock(&page.state);
            let d = state.info.device.as_ref();
            (
                d.and_then(|d| d.user_agent.clone()),
                d.is_some_and(|d| d.mobile),
            )
        };
        let (agent, mobile) = (
            device.as_ref().and_then(|d| d.user_agent.clone()),
            device.as_ref().is_some_and(|d| d.mobile),
        );
        {
            let mut state = lock(&page.state);
            state.info.device = device;
            state.shape();
        }
        let s = Some(page.session.as_str());
        if mobile != mobile_before {
            let _ = browser
                .cdp
                .send(
                    "Emulation.setTouchEmulationEnabled",
                    json!({ "enabled": mobile, "maxTouchPoints": if mobile { 5 } else { 1 } }),
                    s,
                )
                .await;
        }
        browser.reshape(page).await?;
        if agent != agent_before {
            browser.set_user_agent(page, agent.as_deref()).await;
            // Sites pick their layout from the user agent when they load.
            if !lock(&page.state).info.url.is_empty() {
                let _ = browser.cdp.send("Page.reload", json!({}), s).await;
            }
        }
        Ok(())
    }
}

impl Pages {
    pub async fn clear_data(
        &self,
        id: &PageId,
        cookies: bool,
        cache: bool,
    ) -> Result<(), ErrorBody> {
        let (browser, page) = self.tab(id)?;
        let s = Some(page.session.as_str());
        if cookies {
            browser
                .cdp
                .send("Network.clearBrowserCookies", json!({}), s)
                .await
                .map_err(internal)?;
            // And what the open site keeps besides cookies.
            let url = lock(&page.state).info.url.clone();
            if let Some(origin) = origin(&url) {
                let _ = browser
                    .cdp
                    .send(
                        "Storage.clearDataForOrigin",
                        json!({ "origin": origin, "storageTypes": "all" }),
                        None,
                    )
                    .await;
            }
        }
        if cache {
            browser
                .cdp
                .send("Network.clearBrowserCache", json!({}), s)
                .await
                .map_err(internal)?;
        }
        Ok(())
    }

    /// A PNG of the page as it shows, at its scale.
    pub async fn capture(&self, id: &PageId) -> Result<Vec<u8>, ErrorBody> {
        let (browser, page) = self.tab(id)?;
        let _shaping = page.shaping.lock().await;
        let shot = browser
            .cdp
            .send(
                "Page.captureScreenshot",
                json!({ "format": "png" }),
                Some(&page.session),
            )
            .await
            .map_err(internal)?;
        base64::engine::general_purpose::STANDARD
            .decode(shot["data"].as_str().unwrap_or_default())
            .map_err(|e| internal(format!("the browser's picture: {e}")))
    }

    /// Turn annotation mode on or off. While it is on, notes left on the
    /// page go to whoever touched it last.
    pub async fn annotate(&self, id: &PageId, on: bool) -> Result<(), ErrorBody> {
        let (browser, page) = self.tab(id)?;
        let was = std::mem::replace(&mut lock(&page.state).info.annotating, on);
        if was == on {
            return Ok(());
        }
        browser.announce(&page);
        if on {
            tokio::spawn(browser.clone().collect_notes(page.clone()));
        } else {
            let _ = browser
                .notes_script(
                    &page,
                    "if (window.__loftAnnotate) window.__loftAnnotate.stop();",
                )
                .await;
        }
        Ok(())
    }

    /// Take note `n`'s pin off the page, or all of them.
    pub async fn unpin(&self, id: &PageId, n: Option<u32>) -> Result<(), ErrorBody> {
        let (browser, page) = self.tab(id)?;
        let script = match n {
            Some(n) => format!(
                "if (window.__loftAnnotate && window.__loftAnnotate.remove) window.__loftAnnotate.remove({n});"
            ),
            None => "if (window.__loftAnnotate && window.__loftAnnotate.clear) window.__loftAnnotate.clear();".to_owned(),
        };
        browser.notes_script(&page, &script).await.map(|_| ())
    }
}

impl Browser {
    /// Tell the page's viewers its state changed.
    pub(super) fn announce(&self, page: &Page) {
        let _ = self.bus.send(BusMsg::PageState {
            id: page.id.clone(),
        });
    }

    /// The user agent a page presents: a device's, or an ordinary Chrome's
    /// (a headless one names itself so, and some sites turn it away).
    pub(super) async fn set_user_agent(&self, page: &Page, device: Option<&str>) {
        let agent = match device {
            Some(agent) => agent.to_owned(),
            None => match self.ordinary_agent().await {
                Some(agent) => agent,
                None => return,
            },
        };
        let _ = self
            .cdp
            .send(
                "Emulation.setUserAgentOverride",
                json!({ "userAgent": agent }),
                Some(&page.session),
            )
            .await;
    }

    async fn ordinary_agent(&self) -> Option<String> {
        if let Some(agent) = lock(&self.user_agent).clone() {
            return Some(agent);
        }
        let version = self
            .cdp
            .send("Browser.getVersion", json!({}), None)
            .await
            .ok()?;
        let agent = version["userAgent"]
            .as_str()?
            .replace("HeadlessChrome/", "Chrome/");
        *lock(&self.user_agent) = Some(agent.clone());
        Some(agent)
    }

    /// Run `script` in the notes' world of the page's current document.
    async fn notes_script(&self, page: &Page, script: &str) -> Result<Value, ErrorBody> {
        let s = Some(page.session.as_str());
        let known = lock(&page.state).main_frame.clone();
        let frame = match known {
            Some(frame) => frame,
            None => self
                .cdp
                .send("Page.getFrameTree", json!({}), s)
                .await
                .map_err(internal)?["frameTree"]["frame"]["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        };
        // The same name gives the same world for as long as the document
        // lasts, so what the script installed is there the next time.
        let world = self
            .cdp
            .send(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame, "worldName": NOTES_WORLD }),
                s,
            )
            .await
            .map_err(internal)?;
        let context = world["executionContextId"].clone();
        let result = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": script,
                    "contextId": context,
                    "returnByValue": true,
                }),
                s,
            )
            .await
            .map_err(internal)?;
        if let Some(error) = result.get("exceptionDetails") {
            return Err(internal(format!("annotation script: {error}")));
        }
        Ok(result["result"]["value"].clone())
    }

    /// While annotation mode is on: install it in whatever document the page
    /// has, and pass on the notes left there.
    async fn collect_notes(self: Arc<Self>, page: Arc<Page>) {
        let script = format!(
            "{ANNOTATE_JS}\nwindow.__loftAnnotate.start();\nJSON.stringify(window.__loftAnnotate.take());"
        );
        loop {
            tokio::time::sleep(NOTES_EVERY).await;
            if self.dead.load(Ordering::Relaxed) || !lock(&self.pages).contains_key(&page.id) {
                return;
            }
            let (on, loading, to) = {
                let state = lock(&page.state);
                (state.info.annotating, state.info.loading, state.actor)
            };
            if !on {
                return;
            }
            if loading {
                continue;
            }
            let Ok(Value::String(text)) = self.notes_script(&page, &script).await else {
                continue;
            };
            let notes: Vec<PageNote> = serde_json::from_str(&text).unwrap_or_default();
            for note in notes {
                let _ = self.bus.send(BusMsg::PageAction {
                    id: page.id.clone(),
                    action: PageAction::Note { note },
                    to,
                });
            }
        }
    }
}

/// Whether something to type into has the focus, and where the text
/// fields in view are, in the viewport as viewers see it (a page shown
/// magnified or shrunk, as a phone's is, is offset and scaled).
const FIELDS_SCRIPT: &str = r#"(() => {
  const NOT_TEXT = /^(button|submit|reset|checkbox|radio|range|color|file|hidden|image)$/i;
  const typed = (el) => !!el && (el.isContentEditable
    || (el.localName === 'textarea' && !el.readOnly && !el.disabled)
    || (el.localName === 'input' && !NOT_TEXT.test(el.type) && !el.readOnly && !el.disabled)
    || el.getAttribute('role') === 'textbox');
  let el = document.activeElement;
  while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
  const editing = typed(el);
  const vv = window.visualViewport || { offsetLeft: 0, offsetTop: 0, scale: 1, width: innerWidth, height: innerHeight };
  const fields = [];
  const all = document.querySelectorAll('input, textarea, [contenteditable=""], [contenteditable="true"], [role="textbox"]');
  for (const f of all) {
    if (fields.length >= 64) break;
    if (!typed(f)) continue;
    const r = f.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) continue;
    const x = (r.left - vv.offsetLeft) * vv.scale, y = (r.top - vv.offsetTop) * vv.scale;
    const w = r.width * vv.scale, h = r.height * vv.scale;
    if (x + w < 0 || y + h < 0 || x > vv.width * vv.scale || y > vv.height * vv.scale) continue;
    fields.push({ x, y, width: w, height: h });
  }
  return { editing, fields };
})()"#;

/// The pointer a page wants at a point of its viewport: what its CSS asks
/// for there, or for `auto` what a browser would choose (a hand on a link,
/// an I-beam in a field and on text, an arrow elsewhere). As the views'
/// bridge works it out (`loft-view/src/js/bridge.js`), but also into open
/// shadow roots and same-site frames.
const CURSOR_SCRIPT: &str = r#"((x, y) => {
  let doc = document, el = null;
  for (let depth = 0; depth < 8; depth++) {
    el = doc.elementFromPoint(x, y);
    while (el && el.shadowRoot) {
      const inner = el.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === el) break;
      el = inner;
    }
    if (!el || !/^(IFRAME|FRAME)$/.test(el.tagName)) break;
    let inner = null;
    try { inner = el.contentDocument; } catch (_) {}
    if (!inner) return 'default';
    const r = el.getBoundingClientRect();
    x -= r.left + el.clientLeft; y -= r.top + el.clientTop;
    doc = inner;
  }
  if (!el) return 'default';
  const view = el.ownerDocument.defaultView || window;
  const style = view.getComputedStyle(el);
  const named = style.cursor.split(',').pop().trim();
  if (named && named !== 'auto') return named;
  if (el.closest('a[href]')) return 'pointer';
  const field = el.tagName === 'TEXTAREA' || (el.tagName === 'INPUT'
    && !/^(button|submit|reset|checkbox|radio|range|color|file|image|hidden)$/i.test(el.type));
  if (field) return el.disabled ? 'default' : 'text';
  if (el.isContentEditable) return 'text';
  if (style.userSelect === 'none') return 'default';
  const caret = doc.caretRangeFromPoint ? doc.caretRangeFromPoint(x, y) : null;
  const node = caret ? caret.startContainer : null;
  if (node && node.nodeType === Node.TEXT_NODE) {
    const run = doc.createRange();
    run.selectNodeContents(node);
    for (const box of run.getClientRects()) {
      if (x >= box.left && x <= box.right && y >= box.top && y <= box.bottom) return 'text';
    }
  }
  return 'default';
})"#;

/// A pointer a page asked for, as viewers are told it: a CSS keyword, the
/// arrow being none.
pub(super) fn pointer_name(asked: Option<&str>) -> &str {
    match asked {
        Some("default" | "auto") | None => "",
        Some(name) if name.len() <= 24 && name.is_ascii() => name,
        Some(_) => "",
    }
}

impl Browser {
    /// Keep the page's wanted pointer up to date with where the viewer's
    /// pointer is: looked up beside the input, never holding it up, and
    /// only at the latest place when the pointer moves faster than that.
    pub(super) fn follow_pointer(self: &Arc<Self>, page: &Arc<Page>, at: (f32, f32)) {
        if page.view.is_some() {
            // A view says so itself.
            return;
        }
        {
            let mut state = lock(&page.state);
            state.pointer_at = Some(at);
            if std::mem::replace(&mut state.pointer_checking, true) {
                return;
            }
        }
        let (browser, page) = (self.clone(), page.clone());
        tokio::spawn(async move {
            loop {
                let Some((x, y)) = lock(&page.state).pointer_at.take() else {
                    lock(&page.state).pointer_checking = false;
                    return;
                };
                browser.check_cursor(&page, x, y).await;
                let mut state = lock(&page.state);
                if state.pointer_at.is_none() {
                    state.pointer_checking = false;
                    return;
                }
            }
        });
    }

    async fn check_cursor(&self, page: &Page, x: f32, y: f32) {
        let expression = format!("{CURSOR_SCRIPT}({x}, {y})");
        let Ok(reply) = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
                Some(&page.session),
            )
            .await
        else {
            return;
        };
        let wanted = pointer_name(reply["result"]["value"].as_str());
        let changed = {
            let mut state = lock(&page.state);
            let changed = state.info.cursor != wanted;
            if changed {
                state.info.cursor = wanted.to_owned();
            }
            changed
        };
        if changed {
            self.announce(page);
        }
    }

    /// Look again at what can be typed into on the page, and tell its
    /// viewers if that changed (a touch device only shows its keyboard for
    /// a tap on a text field).
    pub(super) async fn check_fields(&self, page: &Page) {
        if page.view.is_some() {
            return;
        }
        let Ok(reply) = self
            .cdp
            .send(
                "Runtime.evaluate",
                json!({ "expression": FIELDS_SCRIPT, "returnByValue": true }),
                Some(&page.session),
            )
            .await
        else {
            return;
        };
        let value = &reply["result"]["value"];
        let editing = value["editing"].as_bool().unwrap_or(false);
        let fields: Vec<PageRect> =
            serde_json::from_value(value["fields"].clone()).unwrap_or_default();
        let changed = {
            let mut state = lock(&page.state);
            let changed = state.info.editing != editing || state.info.fields != fields;
            state.info.editing = editing;
            state.info.fields = fields;
            changed
        };
        if changed {
            self.announce(page);
        }
    }
}

/// `https://example.com` for a page's address; none for a blank page.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pages_origin() {
        assert_eq!(
            origin("https://example.com/a?b#c").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            origin("http://localhost:3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(origin(""), None);
        assert_eq!(origin("about:blank"), None);
    }

    #[test]
    fn notes_read_as_the_script_leaves_them() {
        let text = r##"[{"kind":"element","selector":"#go","note":"bigger","label":"Go","tag":"button","n":1,"url":"https://x.y/","title":"X"},
            {"kind":"area","selector":"","note":"this","n":2,"url":"https://x.y/","title":"X","rect":{"x":1,"y":2,"width":3,"height":4}}]"##;
        let notes: Vec<PageNote> = serde_json::from_str(text).unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].selector, "#go");
        assert_eq!(notes[1].rect.unwrap().height, 4.0);
    }
}

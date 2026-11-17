//! Frames of other sites in an agent's page: what the helpers' world (in
//! the top document) can't read or reach.
//!
//! - A frame of the page's own site is read and used from the top document
//!   (`agent.js` walks into it). Another site's is not reachable from there:
//!   in the same renderer it is a frame of the page's own session; under
//!   site isolation it is a target of its own, attached to here
//!   (`Target.setAutoAttach`, iframes only, the first time an agent reads
//!   a page) and driven over its own session.
//! - Each such frame is numbered for the page's life (`n`, from 1), and
//!   its `<iframe>` is marked with that number in the helpers' world (a
//!   `WeakMap`, nothing in the page). The outline puts what the frame holds
//!   where its `<iframe>` is, its refs counted from `n` × [`FRAME_REFS`],
//!   so a ref says which frame it is in.
//! - A command on such a ref runs its helper in the frame's own world, and
//!   real input aimed there is moved by where the `<iframe>` is.
//! - Network and console captures follow the same frames, over the same
//!   sessions (`capture/cross_site.rs`).
//!
//! At most [`MAX_FRAMES`] frames are read per outline, and only one level
//! down, which keeps an outline short and quick.

use super::automation::{Agent, exception_message};
use super::lock;
use loft_page_script::{BrowserError, SCRIPT_TIMEOUT, script};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Refs of the n-th frame start at n × this.
pub const FRAME_REFS: u32 = 100_000;
/// Frames of other sites read per outline.
pub const MAX_FRAMES: usize = 8;
/// The world helpers run in (as in the top document).
const WORLD: &str = "agentloft";

/// A frame, and the session it is driven over.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub id: String,
    /// `None`: the page's own session (a frame in the same renderer).
    pub session: Option<String>,
}

/// The page's frames of other sites, as far as its agent knows them.
#[derive(Debug, Default)]
pub struct Frames {
    /// `Target.setAutoAttach` was asked for.
    attached: bool,
    /// Frames of their own (target id = frame id) → their session.
    children: HashMap<String, String>,
    /// Frame ids by number (`n` - 1); never reused.
    numbered: Vec<String>,
    /// The helpers' world in a frame, once made.
    worlds: HashMap<String, i64>,
}

impl Frames {
    /// `Target.attachedToTarget` / `detachedFromTarget` on the page's session.
    pub fn observe(&mut self, method: &str, params: &Value) {
        let session = params["sessionId"].as_str().unwrap_or_default();
        if method == "Target.attachedToTarget" {
            let info = &params["targetInfo"];
            if info["type"] == "iframe"
                && let Some(target) = info["targetId"].as_str()
            {
                self.children.insert(target.to_owned(), session.to_owned());
                self.worlds.remove(target);
            }
        } else {
            self.children.retain(|_, s| s != session);
        }
    }

    /// The number of a frame, given one if it has none.
    fn number(&mut self, id: &str) -> u32 {
        match self.numbered.iter().position(|f| f == id) {
            Some(at) => at as u32 + 1,
            None => {
                self.numbered.push(id.to_owned());
                self.numbered.len() as u32
            }
        }
    }

    /// How many frames of their own are attached.
    #[cfg(test)]
    pub fn attached(&self) -> usize {
        self.children.len()
    }

    fn frame(&self, n: u32) -> Option<Frame> {
        let id = self.numbered.get((n as usize).checked_sub(1)?)?.clone();
        let session = self.children.get(&id).cloned();
        Some(Frame { id, session })
    }
}

/// The frame a ref is in (`None`: the top document).
pub fn frame_of(ref_id: Option<u32>) -> Option<u32> {
    ref_id.map(|r| r / FRAME_REFS).filter(|n| *n > 0)
}

/// `Target.setAutoAttach`'s parameters: frames of other sites only (popups
/// are taken from `Target.targetCreated`, `agents.rs`). With `paused`, a
/// frame made from then on waits for `Runtime.runIfWaitingForDebugger`
/// (captures turn their domains on in it first, `capture/cross_site.rs`).
pub(super) fn auto_attach(paused: bool) -> Value {
    json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": paused,
        "flatten": true,
        "filter": [{ "type": "iframe", "exclude": false }],
    })
}

impl Agent<'_> {
    /// Attach to the page's frames of other sites, the first time.
    async fn attach_frames(&self) {
        if lock(&self.driver.frames).attached {
            return;
        }
        self.auto_attach_frames(false).await;
    }

    /// Attach to the page's frames of other sites (`paused`: see
    /// [`auto_attach`]). The first time, waits a moment for the frames
    /// already there, announced right after.
    pub(super) async fn auto_attach_frames(&self, paused: bool) {
        let first = !std::mem::replace(&mut lock(&self.driver.frames).attached, true);
        let _ = self
            .send(
                "Target.setAutoAttach",
                auto_attach(paused),
                SCRIPT_TIMEOUT,
                "attaching to the page's frames",
            )
            .await;
        if first {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Number the frames below the top document that it can't read, and
    /// mark their `<iframe>`s with their numbers in the helpers' world.
    /// Returns how many there are.
    pub(super) async fn mark_frames(&self) -> usize {
        self.attach_frames().await;
        let Ok(tree) = self
            .send(
                "Page.getFrameTree",
                json!({}),
                SCRIPT_TIMEOUT,
                "reading the page's frames",
            )
            .await
        else {
            return 0;
        };
        let mut ids: Vec<String> = tree["frameTree"]["childFrames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["frame"]["id"].as_str().map(str::to_owned))
            .collect();
        for child in lock(&self.driver.frames).children.keys() {
            if !ids.contains(child) {
                ids.push(child.clone());
            }
        }
        let Ok(context) = self.world().await else {
            return 0;
        };
        let mut marked = 0;
        for id in ids.into_iter().take(MAX_FRAMES * 2) {
            // Its <iframe>, in the top document.
            let Ok(owner) = self
                .send(
                    "DOM.getFrameOwner",
                    json!({ "frameId": id }),
                    SCRIPT_TIMEOUT,
                    "finding a frame's element",
                )
                .await
            else {
                continue;
            };
            let Some(node) = owner["backendNodeId"].as_i64() else {
                continue;
            };
            let Ok(resolved) = self
                .send(
                    "DOM.resolveNode",
                    json!({ "backendNodeId": node, "executionContextId": context }),
                    SCRIPT_TIMEOUT,
                    "finding a frame's element",
                )
                .await
            else {
                continue;
            };
            let Some(object) = resolved["object"]["objectId"].as_str() else {
                continue;
            };
            let n = lock(&self.driver.frames).number(&id);
            let _ = self
                .send(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": object,
                        "functionDeclaration": MARK_FRAME,
                        "arguments": [{ "value": n }],
                        "returnByValue": true,
                    }),
                    SCRIPT_TIMEOUT,
                    "marking a frame",
                )
                .await;
            let _ = self
                .send(
                    "Runtime.releaseObject",
                    json!({ "objectId": object }),
                    SCRIPT_TIMEOUT,
                    "marking a frame",
                )
                .await;
            marked += 1;
        }
        marked
    }

    /// The helpers' world in frame `n`, made if need be.
    async fn frame_world(&self, n: u32, fresh: bool) -> Result<(Frame, i64), BrowserError> {
        let frame = lock(&self.driver.frames).frame(n).ok_or_else(|| {
            BrowserError::Action(format!(
                "frame {n} is gone (the page changed); take a new snapshot"
            ))
        })?;
        if !fresh && let Some(context) = lock(&self.driver.frames).worlds.get(&frame.id) {
            return Ok((frame, *context));
        }
        let session = frame.session.as_deref().unwrap_or(&self.page.session);
        let made = self
            .send_on(
                session,
                "Page.createIsolatedWorld",
                json!({ "frameId": frame.id, "worldName": WORLD }),
                SCRIPT_TIMEOUT,
                "preparing a frame for the automation script",
            )
            .await?;
        let context = made["executionContextId"].as_i64().ok_or_else(|| {
            BrowserError::Script("the frame has no room for the automation script".into())
        })?;
        lock(&self.driver.frames)
            .worlds
            .insert(frame.id.clone(), context);
        Ok((frame, context))
    }

    /// Run a helper body in frame `n`'s world and parse its JSON.
    pub(super) async fn frame_helper(
        &self,
        n: u32,
        helper: loft_page_script::Helper,
        mut args: Value,
    ) -> Result<Value, BrowserError> {
        args["refBase"] = json!(n * FRAME_REFS);
        let body = script::helper_call(helper, &args);
        let mut fresh = false;
        loop {
            let (frame, context) = self.frame_world(n, fresh).await?;
            let session = frame.session.as_deref().unwrap_or(&self.page.session);
            let reply = self
                .send_on(
                    session,
                    "Runtime.evaluate",
                    json!({
                        "expression": script::as_expression(&body),
                        "awaitPromise": true,
                        "returnByValue": true,
                        "userGesture": true,
                        "contextId": context,
                    }),
                    SCRIPT_TIMEOUT,
                    "waiting for a frame to run the automation script",
                )
                .await;
            let reply = match reply {
                Err(BrowserError::Action(err))
                    if !fresh
                        && (err.contains("Cannot find context")
                            || err.contains("context was destroyed")) =>
                {
                    fresh = true;
                    continue;
                }
                other => other?,
            };
            if let Some(details) = reply.get("exceptionDetails") {
                return Err(BrowserError::Script(exception_message(details)));
            }
            let text = reply["result"]["value"].as_str().ok_or_else(|| {
                BrowserError::Script("the automation script returned nothing".into())
            })?;
            return serde_json::from_str(text)
                .map_err(|e| BrowserError::Script(format!("unreadable automation result: {e}")));
        }
    }

    /// Where frame `n`'s `<iframe>` content starts in the top document's
    /// viewport (scrolled into view first), and whether `at` (a point of
    /// the frame's own) is covered there by something of the top document.
    pub(super) async fn frame_offset(
        &self,
        n: u32,
        at: Option<(f64, f64)>,
    ) -> Result<Value, BrowserError> {
        let args = json!({
            "frame": n,
            "point": at.map(|(x, y)| json!({ "x": x, "y": y })),
        });
        loft_page_script::action_result(
            self.helper(loft_page_script::Helper::FrameOffset, args)
                .await?,
            None,
        )
    }
}

/// Marks an `<iframe>` with its frame's number in the helpers' world.
const MARK_FRAME: &str = "function (n) {
  const state = globalThis.__loftAgent ||
    (globalThis.__loftAgent = { nextRef: 1, refs: new Map(), ids: new WeakMap() });
  (state.frames || (state.frames = new WeakMap())).set(this, n);
  return true;
}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_keep_their_numbers_and_sessions() {
        let mut frames = Frames::default();
        frames.observe(
            "Target.attachedToTarget",
            &json!({ "sessionId": "S1", "targetInfo": { "type": "iframe", "targetId": "F1" } }),
        );
        // Workers and the like are not frames.
        frames.observe(
            "Target.attachedToTarget",
            &json!({ "sessionId": "S2", "targetInfo": { "type": "worker", "targetId": "W" } }),
        );
        assert_eq!(frames.number("F0"), 1);
        assert_eq!(frames.number("F1"), 2);
        assert_eq!(frames.number("F0"), 1);
        assert_eq!(
            frames.frame(2),
            Some(Frame {
                id: "F1".into(),
                session: Some("S1".into())
            })
        );
        assert_eq!(frames.frame(1).unwrap().session, None);
        frames.observe("Target.detachedFromTarget", &json!({ "sessionId": "S1" }));
        assert_eq!(frames.frame(2).unwrap().session, None);
        assert_eq!(frames.frame(3), None);
        assert_eq!(frame_of(Some(3)), None);
        assert_eq!(frame_of(Some(2 * FRAME_REFS + 7)), Some(2));
        assert_eq!(frame_of(None), None);
    }
}

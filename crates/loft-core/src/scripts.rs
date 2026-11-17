//! Agents' browser scripts (`browser_execute`, ADR-0017 §10): JavaScript
//! run by `loft-page-exec` in the daemon, whose every step is a browser
//! command on one of the thread's tabs, as the plain `browser_*` tools'
//! are.
//!
//! - One script at a time per tab: they share its `state`, which is kept
//!   here (in memory, for as long as the tab is open) between runs.
//! - A few scripts at a time in the whole daemon: each has a thread and up
//!   to 64 MB of its own.

use crate::{Core, CoreResult, lock, pages};
use loft_page_exec::{Script, TabInfo, Tabs};
use loft_page_script::{BrowserCommand, EXECUTE_TIMEOUT, MAX_EXECUTE_TIMEOUT};
use loft_protocol::{ErrorBody, ErrorCode, PageId, Response, ThreadId};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Scripts running at once in the daemon.
const AT_ONCE: usize = 8;

pub(crate) struct Scripts {
    /// Each tab's `state` as JSON, held while a script runs there.
    states: Mutex<HashMap<PageId, Arc<tokio::sync::Mutex<String>>>>,
    running: tokio::sync::Semaphore,
}

impl Default for Scripts {
    fn default() -> Self {
        Self {
            states: Mutex::default(),
            running: tokio::sync::Semaphore::new(AT_ONCE),
        }
    }
}

impl Scripts {
    /// The tab's state slot, forgetting closed tabs'.
    fn slot(&self, pages: &pages::Pages, tab: &PageId) -> Arc<tokio::sync::Mutex<String>> {
        let mut states = lock(&self.states);
        states.retain(|id, _| pages.info(id).is_some());
        states
            .entry(tab.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new("{}".into())))
            .clone()
    }
}

/// The thread's tabs, as a script reaches them.
struct ThreadTabs {
    core: Arc<Core>,
    thread: ThreadId,
}

impl ThreadTabs {
    fn page(&self, tab: &str) -> Result<PageId, String> {
        let id = PageId(tab.to_owned());
        match self.core.pages.info(&id) {
            Some(info) if info.thread_id == self.thread && !info.view => Ok(id),
            _ => Err(format!("this thread has no tab {tab}")),
        }
    }
}

impl Tabs for ThreadTabs {
    fn call<'a>(
        &'a self,
        tab: &'a str,
        command: BrowserCommand,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            // The same way in as the plain tools: tabs, handoffs and status
            // work too, under the tab's limits.
            // What the page's own JavaScript returned is the page's: left
            // as it is.
            let page_value = matches!(
                command,
                BrowserCommand::Eval { .. }
                    | BrowserCommand::EvalOn { .. }
                    | BrowserCommand::WebMcp { .. }
            );
            let reply = self
                .core
                .browser_call(&self.thread, Some(tab), command)
                .await
                .map_err(|error| error.message)?;
            let mut value = reply.value;
            // Which tab it ran in (a tab `tabs new` opened, for one),
            // and what it opened meanwhile.
            if let Value::Object(map) = &mut value
                && !page_value
            {
                if let Some(ran) = &reply.tab {
                    map.entry("tab")
                        .or_insert_with(|| Value::String(ran.id.0.clone()));
                }
                if !reply.new_tabs.is_empty() {
                    map.entry("new_tabs").or_insert_with(|| {
                        serde_json::to_value(&reply.new_tabs).unwrap_or(Value::Null)
                    });
                }
                if !reply.notes.is_empty() {
                    map.entry("notes").or_insert_with(|| {
                        serde_json::to_value(&reply.notes).unwrap_or(Value::Null)
                    });
                }
            }
            Ok(value)
        })
    }

    fn info(&self, tab: &str) -> Option<TabInfo> {
        let info = self.core.pages.info(&self.page(tab).ok()?)?;
        Some(TabInfo {
            url: info.url,
            title: info.title,
        })
    }
}

impl Core {
    /// `BrowserCommand::Execute`: run an agent's script on one of the
    /// thread's tabs (`tab`, else the one it uses now, opened if it has
    /// none). Answers `Response::Json` with `loft_page_exec::Report`'s
    /// JSON; a script that fails is a result too (`is_error`), not an error.
    pub(crate) async fn browser_execute(
        self: &Arc<Self>,
        thread_id: ThreadId,
        script: String,
        tab: Option<String>,
        timeout_ms: Option<u64>,
    ) -> CoreResult<Response> {
        if script.trim().is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "browser_execute needs a script",
            ));
        }
        let timeout = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(EXECUTE_TIMEOUT)
            .clamp(Duration::from_millis(1), MAX_EXECUTE_TIMEOUT);
        let tabs = ThreadTabs {
            core: self.clone(),
            thread: thread_id.clone(),
        };
        // A tab by its id or the name an agent gave it; else the thread's
        // current one, opened if it has none.
        let page = self.agent_tab(&thread_id, tab.as_deref(), true).await?;
        let started = tokio::time::Instant::now();
        let slot = self.scripts.slot(&self.pages, &page);
        let waited = async {
            let state = slot.lock_owned().await;
            let permit = self.scripts.running.acquire().await;
            (state, permit)
        };
        let Ok((mut state, _permit)) = tokio::time::timeout(timeout, waited).await else {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "the script timed out after {} ms waiting for another script in tab {page} to finish",
                    timeout.as_millis()
                ),
            ));
        };
        let script = Script {
            code: script,
            tab: page.0.clone(),
            state: state.clone(),
            timeout: timeout
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(1)),
        };
        let report = loft_page_exec::run(script, &tabs).await;
        if let Some(saved) = &report.state {
            if report.tab == page.0 {
                *state = saved.clone();
            } else if let Ok(other) = tabs.page(&report.tab) {
                // `page` moved to another tab: its state goes with it.
                drop(state);
                *self.scripts.slot(&self.pages, &other).lock().await = saved.clone();
            }
        }
        Ok(Response::Json {
            value: report.to_json(),
        })
    }
}

//! What an agent may grant through Agentloft's own tools (its MCP server).
//!
//! Those tools are allowed without asking in every permission mode (the
//! agent's CLI is told so), so they must never give an agent more than its
//! thread's permission mode already does. Otherwise an agent in a read-only
//! or asking thread (say, one a web page talked into it) could start a
//! full-access thread, or point its own CLI at a program of its choosing,
//! and run anything without the user seeing a prompt.
//!
//! - A thread or automation an agent starts runs with no more than the
//!   caller's mode.
//! - Settings that change what runs (agents' programs, arguments and
//!   environment; the browser's program; where prices are fetched from)
//!   need a full-access caller, which can run anything anyway.
//! - The default mode for new threads can't be raised above the caller's.

use crate::{Core, CoreResult};
use loft_protocol::{ErrorBody, ErrorCode, HarnessId, PermissionMode, Thread, ThreadId};

/// How much a mode lets an agent do without asking, in order.
pub(crate) fn rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::ReadOnly => 0,
        PermissionMode::AskAll => 1,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::Auto => 3,
        PermissionMode::FullAccess => 4,
    }
}

fn label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "read-only",
        PermissionMode::AskAll => "ask-all",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "full-access",
    }
}

fn refused(message: String) -> ErrorBody {
    ErrorBody::new(ErrorCode::Forbidden, message)
}

/// Settings an agent below full access may change: how Agentloft looks and
/// behaves, and agents' defaults; not which programs run or with what.
fn harmless(key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["ui", ..] | ["worktrees", _] | ["titles", _] | ["threads", _] => true,
        ["browser", "search" | "home" | "human_input"] => true,
        ["projects", "new_dir" | "base_dir"] => true,
        ["harness", _, field] => matches!(
            *field,
            "enabled" | "default_model" | "default_effort" | "hidden_models"
        ),
        _ => false,
    }
}

impl Core {
    /// The mode `harness` runs with when `mode` is unset: the user's
    /// default for it, else asking.
    pub(crate) fn mode_or_default(
        &self,
        harness: &HarnessId,
        mode: Option<PermissionMode>,
    ) -> PermissionMode {
        mode.or_else(|| crate::thread_defaults(&self.config(), harness).1)
            .unwrap_or(PermissionMode::AskAll)
    }

    /// The mode thread `id`'s agent runs with.
    fn caller_mode(&self, id: &ThreadId) -> CoreResult<(Thread, PermissionMode)> {
        let thread = self
            .with_store(|s| s.thread(id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        let mode = self.mode_or_default(&thread.harness, thread.permission_mode);
        Ok((thread, mode))
    }

    /// Refuse a thread (or automation run) with mode `wanted` started by
    /// thread `from`'s agent, if `wanted` is more than the caller has.
    pub(crate) fn check_agent_grants(
        &self,
        from: &ThreadId,
        wanted: PermissionMode,
        what: &str,
    ) -> CoreResult<()> {
        let (_, have) = self.caller_mode(from)?;
        if rank(wanted) > rank(have) {
            return Err(refused(format!(
                "this thread is {}, so {what} can't be {}: an agent can't give more \
                 permissions than it has. Ask the user to start it, or to change this \
                 thread's permissions.",
                label(have),
                label(wanted)
            )));
        }
        Ok(())
    }

    /// Refuse a setting thread `from`'s agent may not change to `value`.
    pub(crate) fn check_agent_setting(
        &self,
        from: &ThreadId,
        key: &str,
        value: &serde_json::Value,
    ) -> CoreResult<()> {
        let (_, have) = self.caller_mode(from)?;
        if have == PermissionMode::FullAccess {
            return Ok(());
        }
        let parts: Vec<&str> = key.split('.').collect();
        if let ["harness", agent, "default_permission_mode"] = parts.as_slice() {
            // Reset: the agent's own default, which asks.
            let wanted = match value {
                serde_json::Value::Null => PermissionMode::AskAll,
                v => serde_json::from_value(v.clone()).map_err(|_| {
                    ErrorBody::new(ErrorCode::BadRequest, format!("`{v}` isn't a mode"))
                })?,
            };
            return self.check_agent_grants(from, wanted, &format!("new {agent} threads' default"));
        }
        if harmless(key) {
            return Ok(());
        }
        Err(refused(format!(
            "`{key}` changes what Agentloft runs, so only an agent with full access may \
             change it (this thread is {}). Ask the user to change it in Settings.",
            label(have)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_runs_is_not_harmless() {
        for key in [
            "harness.claude.path",
            "harness.codex.extra_args",
            "harness.claude.env",
            "harness.claude.default_permission_mode",
            "browser.executable",
            "usage.price_catalog",
            "web.enabled",
            "anything.else",
        ] {
            assert!(!harmless(key), "{key}");
        }
        for key in [
            "ui.theme",
            "ui.keys.new_thread",
            "harness.codex.default_model",
            "browser.home",
            "projects.new_dir",
            "titles.model",
        ] {
            assert!(harmless(key), "{key}");
        }
    }

    #[test]
    fn modes_are_ordered_by_what_they_allow() {
        use PermissionMode::*;
        let order = [ReadOnly, AskAll, AcceptEdits, Auto, FullAccess];
        for pair in order.windows(2) {
            assert!(rank(pair[0]) < rank(pair[1]));
        }
    }
}

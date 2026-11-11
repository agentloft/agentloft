//! Views an agent draws in the chat (ADR-0018).
//!
//! An agent calls `show_ui` with a tree of blocks (charts, tables, stats,
//! inputs, buttons) and the user sees it drawn in the thread, natively. This
//! crate is the part the daemon and the UI share: reading that spec
//! ([`parse`]), the expressions in it ([`expr`]), and a few views to try it
//! with ([`samples`]).

pub mod expr;
pub mod samples;
mod spec;

pub use expr::{Format, Scope, Template};
pub use spec::*;

/// The tool's name on the Agentloft MCP server.
pub const TOOL: &str = "show_ui";

/// Whether a tool call (by the name the agent's harness gives it, such as
/// `mcp__agentloft__show_ui`) is a view.
pub fn is_tool(tool: &str) -> bool {
    tool.strip_suffix(TOOL)
        .is_some_and(|head| head.ends_with("agentloft__"))
}

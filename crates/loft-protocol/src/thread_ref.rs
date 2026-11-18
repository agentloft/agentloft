//! Another thread as context for a message: what the composer's `@` chip
//! is sent as, one line the agent can act on:
//!
//! ```text
//! [Agentloft thread "Title" (thread_id 0193…): read it with the agentloft read_thread tool if it is relevant]
//! ```
//!
//! The core reads it back too: a thread the person named this way in a
//! message of theirs is one that thread's agent may read, whatever its
//! permissions (`loft-core`'s `agent_limits`).

use crate::ThreadId;

pub const OPEN: &str = "[Agentloft thread \"";
pub const ID: &str = "\" (thread_id ";
pub const CLOSE: &str = "): read it with the agentloft read_thread tool if it is relevant]";

/// `line` as a thread reference (its title and thread), if it is exactly one.
pub fn parse(line: &str) -> Option<(&str, ThreadId)> {
    let inner = line.trim().strip_prefix(OPEN)?.strip_suffix(CLOSE)?;
    // The id is last and has no quote in it; the title may.
    let (title, id) = inner.rsplit_once(ID)?;
    let plain = !id.is_empty() && !id.contains(|c: char| c.is_whitespace() || c == '"');
    plain.then(|| (title, ThreadId(id.to_owned())))
}

/// The threads `text` refers to, in order.
pub fn ids(text: &str) -> impl Iterator<Item = ThreadId> + '_ {
    let any = text.contains(OPEN);
    text.lines()
        .filter(move |_| any)
        .filter_map(|l| parse(l).map(|(_, id)| id))
}

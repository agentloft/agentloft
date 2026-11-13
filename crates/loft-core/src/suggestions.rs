//! The next message, guessed: after each completed turn the empty composer
//! offers what the user will likely send next (`threads.suggest_next`).
//! The thread's agent is asked once with the last exchange, the way thread
//! titles are written: Claude on Haiku, others on the thread's model at low
//! effort.
//!
//! Claude Code's own `--prompt-suggestions` isn't used: it backs off
//! account-wide while its guesses go unused (`promptSuggestionUnusedStreak`
//! in `~/.claude.json`), so it rarely answered, and each guess not taken
//! here would have quietened it in the user's own Claude Code too.

use loft_protocol::HarnessId;

/// How much of the user's message the model sees.
const MAX_PROMPT_CHARS: usize = 2_000;
/// How much of the agent's reply (its end, where it says what's left).
const MAX_REPLY_CHARS: usize = 4_000;
/// A longer answer is not a message someone would type next.
const MAX_CHARS: usize = 300;

const PROMPT: &str = "A user is working with a coding agent. Predict the next message the user will send.

Reply with that message only, written the way the user writes: short (usually under 12 words), concrete, and something they would actually want next, such as a follow-up the agent offered, a check of the work (run the tests, try it), or the obvious next step. No quotes, no label, no explanation.

If the agent asked the user a question, answer it the way the user most likely would. If there is no clear next step, reply with nothing at all.";

/// Agents the core can ask for a one-off answer (`Core::ask_once`).
pub(crate) fn can_ask(harness: &HarnessId) -> bool {
    matches!(
        harness.as_str(),
        HarnessId::CLAUDE | HarnessId::CODEX | HarnessId::OPENCODE | HarnessId::GEMINI
    )
}

/// The model to ask with: a small one for Claude, the thread's own for
/// the others (Codex's catalog changes too often to name a small one).
pub(crate) fn model(harness: &HarnessId, thread_model: Option<&str>) -> Option<String> {
    match harness.as_str() {
        HarnessId::CLAUDE => Some("haiku".to_owned()),
        _ => thread_model.map(str::to_owned),
    }
}

/// The question for a turn that began with `prompt` and ended with `reply`.
pub(crate) fn prompt(prompt: &str, reply: &str) -> String {
    let prompt: String = prompt.trim().chars().take(MAX_PROMPT_CHARS).collect();
    let reply = reply.trim();
    let skip = reply.chars().count().saturating_sub(MAX_REPLY_CHARS);
    let reply: String = reply.chars().skip(skip).collect();
    let cut = if skip > 0 { "…" } else { "" };
    let prompt = if prompt.is_empty() {
        "(the agent went on by itself)"
    } else {
        &prompt
    };
    format!("{PROMPT}\n\n<user>\n{prompt}\n</user>\n\n<agent>\n{cut}{reply}\n</agent>")
}

/// The suggestion in an agent's reply, without quotes or a label; `None`
/// when it gave none or something no one would type.
pub(crate) fn clean(reply: &str) -> Option<String> {
    let text = reply.trim();
    let lower = text.to_lowercase();
    let text = ["suggestion:", "next message:", "user:", "message:"]
        .iter()
        .find_map(|label| lower.starts_with(label).then(|| &text[label.len()..]))
        .unwrap_or(text)
        .trim();
    let text = match (text.chars().next(), text.chars().last()) {
        (Some(a), Some(b))
            if text.chars().count() >= 2
                && matches!((a, b), ('"', '"') | ('\'', '\'') | ('`', '`') | ('“', '”')) =>
        {
            text[a.len_utf8()..text.len() - b.len_utf8()].trim()
        }
        _ => text,
    };
    let lower = text.to_lowercase();
    if text.is_empty()
        || text.chars().count() > MAX_CHARS
        || matches!(
            lower.trim_end_matches('.'),
            "none" | "nothing" | "n/a" | "(nothing)" | "(none)" | "done"
        )
    {
        return None;
    }
    Some(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_replies() {
        assert_eq!(clean("Run the tests\n").as_deref(), Some("Run the tests"));
        assert_eq!(
            clean("\"Please add a test for the flag.\"").as_deref(),
            Some("Please add a test for the flag.")
        );
        assert_eq!(
            clean("Suggestion: commit this").as_deref(),
            Some("commit this")
        );
        assert_eq!(clean("  "), None);
        assert_eq!(clean("Nothing."), None);
        assert_eq!(clean("\"\""), None);
        assert_eq!(clean(&"word ".repeat(80)), None);
        // A suggestion may span lines (a list of follow-ups, say).
        assert_eq!(
            clean("Do both:\n- a\n- b").as_deref(),
            Some("Do both:\n- a\n- b")
        );
    }

    #[test]
    fn prompt_has_the_exchange_and_the_end_of_a_long_reply() {
        let p = prompt("  add a --verbose flag ", "Added it. No tests yet.");
        assert!(p.contains("<user>\nadd a --verbose flag\n</user>"));
        assert!(p.ends_with("<agent>\nAdded it. No tests yet.\n</agent>"));
        let long = format!("{}THE END", "x".repeat(10_000));
        let p = prompt("", &long);
        assert!(p.contains("(the agent went on by itself)"));
        assert!(p.contains("<agent>\n…") && p.ends_with("THE END\n</agent>"));
        assert!(p.len() < 6_000);
    }

    #[test]
    fn asks_agents_with_one_off_answers() {
        let codex = HarnessId(HarnessId::CODEX.into());
        assert!(can_ask(&codex));
        assert!(!can_ask(&HarnessId(HarnessId::FAKE.into())));
        assert_eq!(
            model(&HarnessId::claude(), Some("opus")).as_deref(),
            Some("haiku")
        );
        assert_eq!(model(&codex, Some("gpt-x")).as_deref(), Some("gpt-x"));
        assert_eq!(model(&codex, None), None);
    }
}

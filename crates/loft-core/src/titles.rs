//! Thread titles written by an agent: after the first message, a one-off
//! call to a small model names the thread (`[titles]` in config.toml). The
//! first line of the message stands in until it answers, and stays if it
//! can't.

use loft_protocol::{AttachmentRef, HarnessId};

/// The longest title kept.
const MAX_CHARS: usize = 60;
/// How much of the message the model sees.
const MAX_MESSAGE_CHARS: usize = 8_000;

const PROMPT: &str = "Write a title that will help the user recognize this coding-agent thread weeks later.
Reply with the title only: no quotes, no label, no explanation.

Before answering, silently reduce the request to:
- Subject: what system, feature, or problem is this really about?
- Outcome: what does the user ultimately want to understand or change?
- Incidental instructions: what only describes how the agent should do the work?

Title the subject and outcome. Discard incidental instructions.

Rules:
- 3-8 words, fewer than 40 characters.
- A compact noun phrase or clear action phrase, in sentence case.
- Capture the umbrella goal when the request lists several symptoms or steps.
- Models, tools, output formats and process instructions don't belong in the title unless they are the topic.
- For reviews, name what is reviewed and the concern; for research, name the question's domain.
- Do not claim the work is done. Do not copy and truncate the user's message.
- No quotes, labels, filler, or trailing punctuation.
- Write the title in the language of the user's message.";

/// Which agent and model write titles for a thread on `thread_harness`;
/// `None` when titles aren't generated for it.
pub(crate) fn writer(
    cfg: &loft_config::TitlesConfig,
    thread_harness: &HarnessId,
) -> Option<(HarnessId, Option<String>)> {
    if !cfg.generate {
        return None;
    }
    let harness = match cfg.agent.trim() {
        "" | "thread" => thread_harness.clone(),
        other => HarnessId(other.to_owned()),
    };
    if !matches!(
        harness.as_str(),
        HarnessId::CLAUDE | HarnessId::CODEX | HarnessId::OPENCODE | HarnessId::GEMINI
    ) {
        return None;
    }
    let model = match cfg.model.trim() {
        "" | "auto" => (harness.as_str() == HarnessId::CLAUDE).then(|| "haiku".to_owned()),
        m => Some(m.to_owned()),
    };
    Some((harness, model))
}

/// The prompt for a thread whose first message is `message`.
pub(crate) fn prompt(message: &str, attachments: &[AttachmentRef], instructions: &str) -> String {
    let mut message: String = message.trim().chars().take(MAX_MESSAGE_CHARS).collect();
    if message.is_empty() {
        message = "(no text)".into();
    }
    let mut out = format!("{PROMPT}\n\nUser message:\n{message}");
    if !attachments.is_empty() {
        out.push_str("\n\nAttached images: ");
        let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
        out.push_str(&names.join(", "));
    }
    let instructions = instructions.trim();
    if !instructions.is_empty() {
        out.push_str("\n\nThe user's own instructions for titles (these take precedence):\n");
        out.push_str(instructions);
    }
    out
}

/// The title in an agent's reply: its first line without quotes, labels or
/// trailing punctuation; `None` when nothing usable is left.
pub(crate) fn clean(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
        .unwrap_or(line);
    let line = line.trim_start_matches('#').trim();
    // Quotes and trailing punctuation, in any order (`"Title".`).
    let line = line
        .trim_start_matches(['"', '\'', '`', '*', '“', '‘', ' '])
        .trim_end_matches(['"', '\'', '`', '*', '”', '’', '.', '!', ':', ';', ',', ' ']);
    let title = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return None;
    }
    if title.chars().count() <= MAX_CHARS {
        return Some(title);
    }
    let mut cut: String = title.chars().take(MAX_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_replies() {
        assert_eq!(
            clean("Fix sidebar scroll jump\n").as_deref(),
            Some("Fix sidebar scroll jump")
        );
        assert_eq!(
            clean("\n\n\"Thread title generation\".").as_deref(),
            Some("Thread title generation")
        );
        assert_eq!(
            clean("Title: **Dark mode toggle**").as_deref(),
            Some("Dark mode toggle")
        );
        assert_eq!(
            clean("# Web   client   pairing\nmore").as_deref(),
            Some("Web client pairing")
        );
        assert_eq!(clean("  \n\"\"\n"), None);
        let long = clean(&"word ".repeat(30)).unwrap();
        assert!(long.ends_with('…') && long.chars().count() <= MAX_CHARS);
    }

    #[test]
    fn picks_the_writer() {
        let mut cfg = loft_config::TitlesConfig::default();
        let claude = HarnessId::claude();
        assert_eq!(
            writer(&cfg, &claude),
            Some((claude.clone(), Some("haiku".into())))
        );
        let codex = HarnessId(HarnessId::CODEX.into());
        assert_eq!(writer(&cfg, &codex), Some((codex.clone(), None)));
        // Agents that can't answer one-off questions get no title call.
        assert_eq!(writer(&cfg, &HarnessId("fake".into())), None);

        cfg.agent = "codex".into();
        cfg.model = "gpt-5-mini".into();
        assert_eq!(
            writer(&cfg, &claude),
            Some((codex, Some("gpt-5-mini".into())))
        );

        cfg.generate = false;
        assert_eq!(writer(&cfg, &claude), None);
    }

    #[test]
    fn prompt_has_the_message_and_instructions() {
        let p = prompt("  make the sidebar faster ", &[], "Use lowercase.");
        assert!(p.contains("User message:\nmake the sidebar faster"));
        assert!(p.ends_with("Use lowercase."));
        assert!(!prompt("hi", &[], " ").contains("own instructions"));
    }
}

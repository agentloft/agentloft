//! A long thread goes to clients in parts: its last turns first, earlier
//! ones as they scroll back, and a subagent's whole run when its tab opens.
//! A thread with subagents at work for hours holds thousands of steps, most
//! of them in no turn's timeline (their own tab shows them).

use loft_protocol::{Item, ItemBody, ItemId, TurnOutline};
use std::collections::HashMap;

/// A thread with no more items than this goes whole.
const WHOLE_UP_TO: usize = 1500;
/// Top-level items in a page: whole turns, newest first, until this many.
const PAGE_ITEMS: usize = 300;
/// How much of a left-out turn's reply its outline carries, in characters.
const OUTLINE_REPLY: usize = 600;

/// A part of a thread for a client.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Page {
    pub items: Vec<Item>,
    /// Earlier turns are left.
    pub older: bool,
    /// For a thread's first page: the earlier turns, as an outline.
    pub outline: Vec<TurnOutline>,
}

/// What a client gets of `items` (in timeline order): the top-level items
/// of whole turns before `before` (an ordinal; the newest when `None`), a
/// page's worth, and of each subagent among them its latest steps (what
/// its row shows: its current step, its last words, when it last moved).
/// Also whether earlier turns are left, and (for the first page) their
/// outline.
pub(crate) fn page(items: Vec<Item>, before: Option<u64>) -> Page {
    if before.is_none() && items.len() <= WHOLE_UP_TO {
        return Page {
            items,
            ..Page::default()
        };
    }
    let first_page = before.is_none();
    let before = before.unwrap_or(u64::MAX);
    let top: Vec<&Item> = items
        .iter()
        .filter(|i| i.parent_id.is_none() && i.ordinal < before)
        .collect();
    // Whole turns from the end: a run of items of one turn (or of none).
    let mut start = top.len();
    while start > 0 && top.len() - start < PAGE_ITEMS {
        let turn = &top[start - 1].turn_id;
        while start > 0 && &top[start - 1].turn_id == turn {
            start -= 1;
        }
    }
    let Some(first) = top.get(start).map(|i| i.ordinal) else {
        return Page::default();
    };
    let older = start > 0;
    let outline = if first_page {
        outline(&top[..start])
    } else {
        Vec::new()
    };
    let shown: HashMap<&ItemId, ()> = top[start..].iter().map(|i| (&i.id, ())).collect();

    // Per subagent shown: its latest tool call, last message and the step
    // that moved last.
    let mut tails: HashMap<&ItemId, [Option<usize>; 3]> = HashMap::new();
    for (ix, item) in items.iter().enumerate() {
        let Some(parent) = item.parent_id.as_ref().filter(|p| shown.contains_key(p)) else {
            continue;
        };
        let tail = tails.entry(parent).or_default();
        match item.body {
            ItemBody::ToolCall { .. } => tail[0] = Some(ix),
            ItemBody::AssistantMessage { .. } => tail[1] = Some(ix),
            _ => {}
        }
        if tail[2].is_none_or(|t| items[t].updated_at_ms <= item.updated_at_ms) {
            tail[2] = Some(ix);
        }
    }
    let mut keep: Vec<bool> = items
        .iter()
        .map(|i| i.parent_id.is_none() && i.ordinal >= first && i.ordinal < before)
        .collect();
    for ix in tails.values().flatten().flatten() {
        keep[*ix] = true;
    }
    let items = items
        .into_iter()
        .zip(keep)
        .filter_map(|(item, keep)| keep.then_some(item))
        .collect();
    Page {
        items,
        older,
        outline,
    }
}

/// Each message the user sent among `top` (top-level items, in order) and
/// the start of the agent's last reply in its turn.
fn outline(top: &[&Item]) -> Vec<TurnOutline> {
    let mut replies: HashMap<_, &str> = HashMap::new();
    for item in top {
        if let ItemBody::AssistantMessage { text } = &item.body {
            replies.insert(&item.turn_id, text.as_str());
        }
    }
    top.iter()
        .filter(|i| matches!(i.body, ItemBody::UserMessage { .. }))
        .map(|message| TurnOutline {
            message: (*message).clone(),
            reply: message
                .turn_id
                .as_ref()
                .and_then(|_| replies.get(&message.turn_id))
                .map(|r| r.chars().take(OUTLINE_REPLY).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// One subagent's whole run: the steps of the call `of`.
pub(crate) fn steps(items: Vec<Item>, of: &ItemId) -> Vec<Item> {
    items
        .into_iter()
        .filter(|i| i.parent_id.as_ref() == Some(of))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::{ItemStatus, ThreadId, ToolKind, TurnId};

    fn item(ordinal: u64, turn: Option<&TurnId>, parent: Option<&ItemId>, body: ItemBody) -> Item {
        Item {
            id: ItemId::new(),
            thread_id: ThreadId::new(),
            turn_id: turn.cloned(),
            ordinal,
            status: ItemStatus::Completed,
            body,
            updated_at_ms: ordinal as i64,
            parent_id: parent.cloned(),
            started_at_ms: None,
        }
    }

    fn said(text: &str) -> ItemBody {
        ItemBody::AssistantMessage { text: text.into() }
    }

    fn tool(title: &str) -> ItemBody {
        ItemBody::ToolCall {
            tool: "Bash".into(),
            tool_kind: ToolKind::Command,
            title: title.into(),
            input: serde_json::json!({}),
            output: None,
            is_error: false,
        }
    }

    /// 40 turns of 50 top-level items, the last with a subagent that took
    /// 500 steps: the snapshot is the last turns (whole) and the subagent's
    /// latest steps; earlier pages follow on, back to the first turn.
    #[test]
    fn a_long_thread_comes_in_whole_turns_with_subagents_trimmed() {
        let turns: Vec<TurnId> = (0..40).map(|_| TurnId::new()).collect();
        let mut items = Vec::new();
        let mut ordinal = 0;
        let mut next = || {
            ordinal += 1;
            ordinal
        };
        for turn in &turns {
            for k in 0..50 {
                items.push(item(next(), Some(turn), None, said(&format!("{k}"))));
            }
        }
        let agent = item(next(), turns.last(), None, tool("Build the reel"));
        let agent_id = agent.id.clone();
        items.push(agent);
        for k in 0..500 {
            items.push(item(
                next(),
                None,
                Some(&agent_id),
                tool(&format!("step {k}")),
            ));
        }
        items.push(item(next(), None, Some(&agent_id), said("Reel built.")));
        items.push(item(next(), None, Some(&agent_id), tool("last step")));
        let all = items.clone();

        let Page {
            items: first,
            older,
            outline,
        } = page(items, None);
        assert!(older);
        // The earlier 34 turns, as an outline: no messages in this one.
        assert!(outline.is_empty());
        let top: Vec<&Item> = first.iter().filter(|i| i.parent_id.is_none()).collect();
        // Whole turns: 300 reached on the 6th turn from the end (with the
        // subagent call, the last turn has 51).
        assert_eq!(top.len(), 301);
        assert_eq!(top[0].turn_id.as_ref(), Some(&turns[34]));
        let steps: Vec<&Item> = first.iter().filter(|i| i.parent_id.is_some()).collect();
        let titles: Vec<String> = steps
            .iter()
            .map(|i| match &i.body {
                ItemBody::ToolCall { title, .. } => title.clone(),
                ItemBody::AssistantMessage { text } => text.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(titles, ["Reel built.", "last step"]);

        // Scrolling back: page after page, whole turns, to the first.
        let mut before = top[0].ordinal;
        let mut pages = 0;
        loop {
            let Page {
                items: page_items,
                older,
                outline,
            } = page(all.clone(), Some(before));
            assert!(outline.is_empty(), "only the first page has one");
            assert!(
                page_items
                    .iter()
                    .all(|i| i.ordinal < before && i.parent_id.is_none())
            );
            assert_eq!(page_items.len() % 50, 0, "whole turns only");
            before = page_items[0].ordinal;
            pages += 1;
            if !older {
                break;
            }
        }
        assert_eq!(before, 1);
        assert_eq!(pages, 6);

        assert_eq!(steps_of(&all, &agent_id), 502);
    }

    fn steps_of(all: &[Item], of: &ItemId) -> usize {
        steps(all.to_vec(), of).len()
    }

    /// The turns left out of the first page come as an outline: each
    /// message and the start of its turn's last reply.
    #[test]
    fn earlier_turns_come_as_an_outline() {
        let mut items = Vec::new();
        let mut ordinal = 0;
        let turns: Vec<TurnId> = (0..12).map(|_| TurnId::new()).collect();
        for (n, turn) in turns.iter().enumerate() {
            ordinal += 1;
            let ask = ItemBody::UserMessage {
                text: format!("ask {n}"),
                attachments: vec![],
            };
            items.push(item(ordinal, Some(turn), None, ask));
            for k in 0..150 {
                ordinal += 1;
                let body = if k == 149 {
                    said(&format!("answer {n} {}", "x".repeat(1000)))
                } else {
                    tool("step")
                };
                items.push(item(ordinal, Some(turn), None, body));
            }
        }
        let page = page(items, None);
        assert!(page.older);
        // Two turns fill a page (302 items); the ten before are outlined.
        assert_eq!(page.outline.len(), 10);
        let asks: Vec<String> = page
            .outline
            .iter()
            .map(|o| match &o.message.body {
                ItemBody::UserMessage { text, .. } => text.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(asks.first().map(String::as_str), Some("ask 0"));
        assert_eq!(asks.last().map(String::as_str), Some("ask 9"));
        assert!(page.outline[0].reply.starts_with("answer 0 xxx"));
        assert_eq!(page.outline[0].reply.chars().count(), OUTLINE_REPLY);
    }

    #[test]
    fn a_short_thread_goes_whole() {
        let turn = TurnId::new();
        let agent = item(1, Some(&turn), None, tool("agent"));
        let step = item(2, None, Some(&agent.id), tool("step"));
        let items = vec![agent, step];
        assert_eq!(
            page(items.clone(), None),
            Page {
                items,
                ..Page::default()
            }
        );
    }
}

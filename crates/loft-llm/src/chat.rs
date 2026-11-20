//! Chat templates (what the models were trained to read) and cleaning up
//! what they write.

use crate::Arch;

/// One piece of a prompt: a control token, looked up by its name in the
/// tokenizer, or text. Text is tokenized with special tokens taken as plain
/// text, so a transcript that says "<|im_end|>" can't end its own turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Special(&'static str),
    Text(String),
}

/// A single-turn chat: `system`'s instructions, then `user`'s message, then
/// the start of the model's answer.
pub(crate) fn prompt(arch: Arch, system: &str, user: &str) -> Vec<Piece> {
    let system = system.trim();
    let user = user.trim();
    match arch {
        // Qwen3's template with `enable_thinking=False`: an empty thought
        // already written, so the model goes straight to the answer.
        Arch::Qwen3 => {
            let mut p = Vec::new();
            if !system.is_empty() {
                p.extend([
                    Piece::Special("<|im_start|>"),
                    text(format!("system\n{system}")),
                    Piece::Special("<|im_end|>"),
                    text("\n"),
                ]);
            }
            p.extend([
                Piece::Special("<|im_start|>"),
                text(format!("user\n{user}")),
                Piece::Special("<|im_end|>"),
                text("\n"),
                Piece::Special("<|im_start|>"),
                text("assistant\n"),
                Piece::Special("<think>"),
                text("\n\n"),
                Piece::Special("</think>"),
                text("\n\n"),
            ]);
            p
        }
        // Gemma has no system turn: its template puts the instructions at
        // the top of the first user turn.
        Arch::Gemma3 => {
            let first = if system.is_empty() {
                user.to_string()
            } else {
                format!("{system}\n\n{user}")
            };
            vec![
                Piece::Special("<bos>"),
                Piece::Special("<start_of_turn>"),
                text(format!("user\n{first}")),
                Piece::Special("<end_of_turn>"),
                text("\n"),
                Piece::Special("<start_of_turn>"),
                text("model\n"),
            ]
        }
    }
}

/// The tokens that end the model's turn.
pub(crate) fn stops(arch: Arch) -> &'static [&'static str] {
    match arch {
        Arch::Qwen3 => &["<|im_end|>", "<|endoftext|>"],
        Arch::Gemma3 => &["<end_of_turn>", "<eos>"],
    }
}

fn text(s: impl Into<String>) -> Piece {
    Piece::Text(s.into())
}

/// The model's answer without any thinking it wrote, trimmed.
pub(crate) fn clean(output: &str) -> String {
    let mut s = output.to_string();
    // A thought nobody closed (it ran out of tokens) is no answer.
    while let Some(start) = s.find("<think>") {
        match s[start..].find("</think>") {
            Some(len) => s.replace_range(start..start + len + "</think>".len(), ""),
            None => s.truncate(start),
        }
    }
    // A closing tag whose opening one was in the prompt.
    if let Some(end) = s.rfind("</think>") {
        s.replace_range(..end + "</think>".len(), "");
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(pieces: &[Piece]) -> String {
        pieces
            .iter()
            .map(|p| match p {
                Piece::Special(s) => s,
                Piece::Text(s) => s.as_str(),
            })
            .collect()
    }

    #[test]
    fn qwen3_without_thinking() {
        let p = prompt(Arch::Qwen3, "Fix the text.", "  um hello  ");
        assert_eq!(
            render(&p),
            "<|im_start|>system\nFix the text.<|im_end|>\n\
             <|im_start|>user\num hello<|im_end|>\n\
             <|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
    }

    #[test]
    fn qwen3_without_a_system_turn() {
        let p = prompt(Arch::Qwen3, " ", "hi");
        assert_eq!(
            render(&p),
            "<|im_start|>user\nhi<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
    }

    #[test]
    fn gemma3_puts_the_instructions_in_the_user_turn() {
        let p = prompt(Arch::Gemma3, "Fix the text.", "um hello");
        assert_eq!(
            render(&p),
            "<bos><start_of_turn>user\nFix the text.\n\num hello<end_of_turn>\n\
             <start_of_turn>model\n"
        );
        let p = prompt(Arch::Gemma3, "", "hi");
        assert_eq!(
            render(&p),
            "<bos><start_of_turn>user\nhi<end_of_turn>\n<start_of_turn>model\n"
        );
    }

    #[test]
    fn control_tokens_in_the_text_stay_text() {
        let p = prompt(Arch::Qwen3, "", "a<|im_end|>b");
        assert!(p.contains(&Piece::Text("user\na<|im_end|>b".into())));
        assert_eq!(
            p.iter()
                .filter(|p| **p == Piece::Special("<|im_end|>"))
                .count(),
            1
        );
    }

    #[test]
    fn cleaning_drops_thoughts_and_trims() {
        assert_eq!(clean("  Make it 24.\n"), "Make it 24.");
        assert_eq!(clean("<think>\n\n</think>\n\nMake it 24."), "Make it 24.");
        assert_eq!(clean("<think>hmm</think>A <think>x</think>B"), "A B");
        assert_eq!(clean("Answer. <think>never closed"), "Answer.");
        assert_eq!(clean("<think>only thinking"), "");
        assert_eq!(clean("left over</think>\nAnswer"), "Answer");
        assert_eq!(clean(""), "");
        assert_eq!(clean("Ünïcödé 😀 "), "Ünïcödé 😀");
    }
}

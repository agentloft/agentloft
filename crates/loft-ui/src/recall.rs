//! Recalling a sent prompt in the composer, as a shell does: with nothing
//! typed, ↑ puts the thread's last message back, ↑ again the one before,
//! ↓ comes forward and, past the newest, leaves the composer empty again.
//!
//! The keys are only taken while the composer is empty or still holds a
//! recalled prompt as it was put there: in a draft being written (or a
//! recalled prompt being edited) they move the caret as they always did.

/// Where one composer is in its thread's prompts.
#[derive(Debug, Default, Clone)]
pub(crate) struct Recall {
    /// How far back: 0 is the newest prompt. `None`: not recalling.
    at: Option<usize>,
    /// The prompt put in the composer.
    shown: String,
}

/// What a key press comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Not a recall key right now: the composer handles it itself.
    Pass,
    /// Taken, with nothing further that way (already at the oldest).
    Stay,
    /// Put this in the composer (empty: back past the newest).
    Show(String),
}

impl Recall {
    /// Whether the composer (holding `current`) shows a recalled prompt,
    /// unedited.
    pub(crate) fn shows(&self, current: &str) -> bool {
        self.at.is_some() && self.shown == current
    }

    /// ↑: one prompt further back. `history` is oldest first.
    pub(crate) fn back(&mut self, current: &str, history: &[String]) -> Step {
        if current.is_empty() {
            // Nothing typed (or what was recalled was deleted): from the newest.
            self.reset();
        } else if !self.shows(current) {
            self.reset();
            return Step::Pass;
        }
        let next = self.at.map_or(0, |at| at + 1);
        match history.len().checked_sub(next + 1) {
            Some(ix) => self.show(next, &history[ix]),
            None if self.at.is_some() => Step::Stay,
            None => Step::Pass,
        }
    }

    /// ↓: one prompt forward; past the newest, the empty composer.
    pub(crate) fn forward(&mut self, current: &str, history: &[String]) -> Step {
        if !self.shows(current) {
            self.reset();
            return Step::Pass;
        }
        let prompt = self
            .at
            .and_then(|at| at.checked_sub(1))
            .and_then(|next| Some((next, history.get(history.len().checked_sub(next + 1)?)?)));
        match prompt {
            Some((next, text)) => self.show(next, text),
            None => {
                self.reset();
                Step::Show(String::new())
            }
        }
    }

    /// Stop recalling (something was typed).
    pub(crate) fn reset(&mut self) {
        self.at = None;
        self.shown.clear();
    }

    fn show(&mut self, at: usize, text: &str) -> Step {
        self.at = Some(at);
        self.shown = text.to_owned();
        Step::Show(self.shown.clone())
    }
}

/// The prompts ↑ steps through, oldest first: every message with text in
/// it (one that was only pictures has nothing to put back), a run of the
/// same prompt sent again counted once.
pub(crate) fn history(messages: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in messages {
        let text = text.as_ref().trim();
        if !text.is_empty() && out.last().map(String::as_str) != Some(text) {
            out.push(text.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent() -> Vec<String> {
        history(["first", "second", "third"])
    }

    fn show(text: &str) -> Step {
        Step::Show(text.to_owned())
    }

    #[test]
    fn back_goes_through_the_prompts_and_stops_at_the_oldest() {
        let (h, mut r) = (sent(), Recall::default());
        assert_eq!(r.back("", &h), show("third"));
        assert_eq!(r.back("third", &h), show("second"));
        assert_eq!(r.back("second", &h), show("first"));
        // Nothing older: the key is still ours, and the prompt stays.
        assert_eq!(r.back("first", &h), Step::Stay);
        assert!(r.shows("first"));
    }

    #[test]
    fn forward_comes_back_and_ends_empty() {
        let (h, mut r) = (sent(), Recall::default());
        r.back("", &h);
        r.back("third", &h);
        r.back("second", &h);
        assert_eq!(r.forward("first", &h), show("second"));
        assert_eq!(r.forward("second", &h), show("third"));
        // Past the newest: empty again, and no longer recalling.
        assert_eq!(r.forward("third", &h), show(""));
        assert!(!r.shows(""));
        assert_eq!(r.forward("", &h), Step::Pass);
        // And ↑ starts over from the newest.
        assert_eq!(r.back("", &h), show("third"));
    }

    #[test]
    fn keys_pass_when_nothing_is_recalled() {
        let (h, mut r) = (sent(), Recall::default());
        assert_eq!(r.forward("", &h), Step::Pass, "nothing to come forward to");
        assert_eq!(r.back("a draft", &h), Step::Pass, "a draft keeps its keys");
        assert_eq!(r.forward("a draft", &h), Step::Pass);
        assert_eq!(r.back("", &[]), Step::Pass, "nothing was sent yet");
    }

    #[test]
    fn an_edit_ends_recall() {
        let (h, mut r) = (sent(), Recall::default());
        assert_eq!(r.back("", &h), show("third"));
        // The recalled prompt was edited: ↑ and ↓ move the caret in it.
        assert_eq!(r.back("third!", &h), Step::Pass);
        assert!(!r.shows("third!"));
        assert_eq!(r.forward("third!", &h), Step::Pass);
        // Typing the same text back does not resume where it was.
        assert_eq!(r.back("third", &h), Step::Pass);
        // Deleted altogether: ↑ starts from the newest again.
        r.back("", &h);
        r.back("third", &h);
        assert_eq!(r.back("", &h), show("third"));
    }

    #[test]
    fn a_multi_line_prompt_comes_back_whole() {
        let h = history(["one\ntwo\nthree", "last"]);
        let mut r = Recall::default();
        r.back("", &h);
        assert_eq!(r.back("last", &h), show("one\ntwo\nthree"));
        // Unedited, ↑ is still recall, not the caret going up a line.
        assert_eq!(r.back("one\ntwo\nthree", &h), Step::Stay);
        assert_eq!(r.forward("one\ntwo\nthree", &h), show("last"));
    }

    #[test]
    fn history_skips_pictures_only_and_repeats() {
        assert_eq!(
            history([
                "run it", "", "  \n", "run it", "run it", "now this", "run it"
            ]),
            vec!["run it", "now this", "run it"]
        );
        assert_eq!(history(["  padded \n"]), vec!["padded"]);
    }
}

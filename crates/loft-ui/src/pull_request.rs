//! How a thread's pull request is shown: a badge by its branch in the title
//! bar, and the words and colours the details card uses too. One colour
//! says how it stands: checks failing, running or passing, or merged.

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, px,
};
use loft_protocol::{ChecksState, PullRequest, PullRequestState, ReviewDecision};

/// How a pull request stands, in the one colour it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// A check failed.
    Failing,
    /// Checks are queued or running.
    Running,
    /// Every check passed.
    Passing,
    Merged,
    /// Nothing to say in colour: no checks, or closed without merging.
    Quiet,
}

pub fn tone(pr: &PullRequest) -> Tone {
    match pr.state {
        PullRequestState::Merged => Tone::Merged,
        PullRequestState::Closed => Tone::Quiet,
        PullRequestState::Open | PullRequestState::Draft => match pr.checks.rollup() {
            Some(ChecksState::Failing) => Tone::Failing,
            Some(ChecksState::Pending) => Tone::Running,
            Some(ChecksState::Passing) => Tone::Passing,
            None => Tone::Quiet,
        },
    }
}

pub fn color(tone: Tone, p: &Palette) -> Hsla {
    match tone {
        Tone::Failing => p.danger,
        Tone::Running => p.warning,
        Tone::Passing => p.success,
        Tone::Merged => p.info,
        Tone::Quiet => p.muted_foreground,
    }
}

pub fn icon(pr: &PullRequest) -> IconName {
    match pr.state {
        PullRequestState::Merged => IconName::GitMerge,
        _ => IconName::GitPullRequest,
    }
}

pub fn state_label(state: PullRequestState) -> &'static str {
    match state {
        PullRequestState::Open => "Open",
        PullRequestState::Draft => "Draft",
        PullRequestState::Merged => "Merged",
        PullRequestState::Closed => "Closed",
    }
}

pub fn review_label(review: ReviewDecision) -> &'static str {
    match review {
        ReviewDecision::Approved => "Approved",
        ReviewDecision::ChangesRequested => "Changes requested",
        ReviewDecision::ReviewRequired => "Review required",
    }
}

/// "Open · 3 passing, 1 failing, 2 running · Changes requested". Checks
/// and reviews are an open pull request's business: left out once it is
/// merged or closed.
pub fn describe(pr: &PullRequest) -> String {
    let mut parts = vec![state_label(pr.state).to_owned()];
    if pr.is_open() {
        parts.extend(pr.checks.summary());
        parts.extend(pr.review.map(|r| review_label(r).to_owned()));
    }
    parts.join(" · ")
}

/// The badge by the thread's branch: "#42" in the colour of how it stands.
/// Clicking it opens the pull request in the browser.
pub fn badge(pr: &PullRequest, p: &Palette) -> AnyElement {
    let url = pr.url.clone();
    let about = format!("Pull request #{} · {}", pr.number, describe(pr));
    h_flex()
        .id("hdr-pull-request")
        .flex_none()
        .gap_1()
        .items_center()
        .px_2()
        .py(px(3.))
        .rounded(px(7.))
        .text_xs()
        .text_color(color(tone(pr), p))
        .cursor_pointer()
        .hover(|d| d.bg(p.hover))
        .child(Icon::new(icon(pr)).xsmall())
        .child(format!("#{}", pr.number))
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(about.clone()).build(window, cx)
        })
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::PullRequestChecks;

    fn pr(state: PullRequestState, checks: (u32, u32, u32)) -> PullRequest {
        PullRequest {
            number: 42,
            url: "https://github.com/acme/repo/pull/42".into(),
            title: "Add a changelog".into(),
            state,
            checks: PullRequestChecks {
                passing: checks.0,
                failing: checks.1,
                pending: checks.2,
            },
            review: None,
            head: None,
            refreshed_at_ms: 0,
        }
    }

    #[test]
    fn one_colour_for_how_it_stands() {
        use PullRequestState::*;
        // A failure outweighs what is still running, which outweighs passes.
        assert_eq!(tone(&pr(Open, (3, 1, 2))), Tone::Failing);
        assert_eq!(tone(&pr(Open, (3, 0, 2))), Tone::Running);
        assert_eq!(tone(&pr(Draft, (3, 0, 0))), Tone::Passing);
        assert_eq!(tone(&pr(Open, (0, 0, 0))), Tone::Quiet);
        // Once merged or closed, its checks no longer colour it.
        assert_eq!(tone(&pr(Merged, (3, 1, 2))), Tone::Merged);
        assert_eq!(tone(&pr(Closed, (0, 1, 0))), Tone::Quiet);
    }

    #[test]
    fn described_in_a_line() {
        use PullRequestState::*;
        let mut open = pr(Open, (3, 1, 2));
        assert_eq!(describe(&open), "Open · 3 passing, 1 failing, 2 running");
        open.review = Some(ReviewDecision::ChangesRequested);
        assert_eq!(
            describe(&open),
            "Open · 3 passing, 1 failing, 2 running · Changes requested"
        );
        assert_eq!(describe(&pr(Draft, (0, 0, 0))), "Draft");
        assert_eq!(describe(&pr(Open, (0, 0, 1))), "Open · 1 running");
        let mut merged = pr(Merged, (4, 0, 0));
        merged.review = Some(ReviewDecision::Approved);
        assert_eq!(describe(&merged), "Merged");
    }
}

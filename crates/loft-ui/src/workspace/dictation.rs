//! Dictation into the composer: the microphone button (and ⇧⌘D) records,
//! the daemon turns the recording into text (as it comes with a model that
//! transcribes live, `Request::VoiceLive*`, shown above the toolbar while
//! recording; else all at once at the end, `Request::VoiceTranscribe`),
//! optionally tidies it (`Request::VoiceCleanup`), and the text goes where
//! the cursor was when recording began. Nothing is sent to the agent: the
//! text waits in the draft like typed text.

use super::WorkspaceView;
use crate::press::ClaimsPress as _;
use crate::theme::Palette;
use crate::voice::capture::{Recorder, SAMPLE_RATE};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Task, Window, actions, div, px,
};
use loft_protocol::{Bytes, Request, Response, ThreadId};
use std::ops::Range;
use std::time::Duration;

actions!(
    agentloft,
    [
        /// Start dictating into the composer, or finish.
        Dictate
    ]
);

/// Recordings shorter than this were a slip of the finger.
const MIN_SECONDS: f32 = 0.3;
/// How often the level meter and the clock are redrawn while recording.
const TICK: Duration = Duration::from_millis(80);
/// Live transcription is sent the recording in pieces at least this long
/// (samples: a quarter of a second), one piece at a time.
const LIVE_PIECE: usize = SAMPLE_RATE as usize / 4;

pub(super) struct Dictation {
    pub(super) thread_id: ThreadId,
    pub(super) phase: Phase,
    /// The draft and its selection when recording began: the text goes
    /// there, unless the draft changed meanwhile.
    start: (String, Range<usize>),
    /// The level meter's recent readings, newest last.
    levels: Vec<f32>,
    /// Transcription while recording, with a model that does it.
    live: Live,
    /// What live transcription has heard so far: text that stays, and text
    /// that may still change.
    heard: (String, String),
    /// Hold to talk: what is being held; letting go finishes.
    held: Option<Held>,
    _task: Task<()>,
}

/// What a hold-to-talk recording lasts as long as.
#[derive(Clone)]
enum Held {
    /// The shortcut: its key and modifiers.
    Keys(gpui_kit::Keystroke),
    /// The dictation key, a lone modifier.
    Side,
    /// The microphone button, pressed.
    Pointer,
}

/// A press of the dictation key longer than this isn't a tap (toggle).
const TAP: Duration = Duration::from_millis(600);

/// Feeds the dictation key's presses to the window's workspace.
pub(super) fn watch_side_keys(window: &mut Window, cx: &mut Context<WorkspaceView>) -> Task<()> {
    use futures::StreamExt as _;
    let mut events = crate::voice::keys::subscribe();
    cx.spawn_in(window, async move |this, cx| {
        while let Some(event) = events.next().await {
            if this
                .update_in(cx, |this, window, cx| this.side_key(event, window, cx))
                .is_err()
            {
                break;
            }
        }
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Live {
    /// Asked the daemon whether the model transcribes live.
    Asking,
    /// It does: this transcription's id, how much of the recording was
    /// sent, and whether a piece is on its way.
    On { id: u64, sent: usize, sending: bool },
    /// It doesn't (or live transcription failed): the whole recording is
    /// sent at the end.
    Off,
}

pub(super) enum Phase {
    /// Waiting for the microphone (and its permission).
    Starting,
    Recording(Recorder),
    Transcribing,
    /// The transcript is being tidied; Skip puts this in as heard.
    Tidying(String),
}

impl WorkspaceView {
    /// The shortcut, pressed: toggles, or with hold to talk starts (the
    /// key's repeats while it is held change nothing).
    pub(super) fn dictate_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.store.read(cx).voice.hold_to_talk() {
            self.toggle_dictation(window, cx);
            return;
        }
        if self.dictation.is_some() {
            return;
        }
        let os = cx.platform_info().os;
        let keys = crate::keymap::keys_for("dictate", cx).and_then(|keys| {
            let first = keys.split_whitespace().next()?.to_owned();
            gpui_kit::Keystroke::parse_with_os(&first, os).ok()
        });
        self.start_dictation(window, cx);
        if let Some(d) = self.dictation.as_mut() {
            // A shortcut that can't be read back ends like a toggle's.
            d.held = keys.map(Held::Keys);
        }
    }

    /// The dictation key (`[voice] key`, a lone modifier): with hold to
    /// talk it records while held; else a tap toggles. Held with another
    /// key it was a combination (⌥E types a letter), not for us.
    fn side_key(
        &mut self,
        event: crate::voice::keys::KeyEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::voice::keys::{KeyEvent, SideKey};
        let voice = self.store.read(cx).voice.clone();
        let Some(key) = SideKey::parse(&voice.key) else {
            self.side_press = None;
            return;
        };
        tracing::debug!(
            ?event,
            ?key,
            active = window.is_window_active(),
            "dictation key"
        );
        // Every window hears the key; the one in front acts.
        if !window.is_window_active() {
            return;
        }
        let hold = voice.hold_to_talk();
        match event {
            KeyEvent::Down(k) if k == key => {
                self.side_press = Some((web_time::Instant::now(), false));
                if hold && self.dictation.is_none() {
                    self.start_dictation(window, cx);
                    if let Some(d) = self.dictation.as_mut() {
                        d.held = Some(Held::Side);
                    }
                }
            }
            KeyEvent::Other => {
                if let Some((_, chord)) = self.side_press.as_mut()
                    && !*chord
                {
                    *chord = true;
                    let ours = matches!(
                        self.dictation.as_ref().and_then(|d| d.held.as_ref()),
                        Some(Held::Side)
                    );
                    if ours {
                        self.cancel_dictation(cx);
                    }
                }
            }
            KeyEvent::Up(k) if k == key => {
                let Some((since, chord)) = self.side_press.take() else {
                    return;
                };
                if hold {
                    if matches!(
                        self.dictation.as_ref().and_then(|d| d.held.as_ref()),
                        Some(Held::Side)
                    ) {
                        self.let_go(window, cx);
                    }
                } else if !chord && since.elapsed() < TAP {
                    self.toggle_dictation(window, cx);
                }
            }
            _ => {}
        }
    }

    /// The microphone button pressed, with hold to talk.
    fn hold_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dictation.is_some() {
            return;
        }
        self.start_dictation(window, cx);
        if let Some(d) = self.dictation.as_mut() {
            d.held = Some(Held::Pointer);
        }
    }

    /// A key let go: the end of a hold-to-talk shortcut.
    pub(super) fn dictation_key_up(
        &mut self,
        key: &gpui_kit::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Held::Keys(keys)) = self.dictation.as_ref().and_then(|d| d.held.clone())
            && key.key.eq_ignore_ascii_case(&keys.key)
        {
            self.let_go(window, cx);
        }
    }

    /// Modifiers changed: a hold-to-talk shortcut ends when one of its own
    /// is let go.
    pub(super) fn dictation_modifiers(
        &mut self,
        now: &gpui_kit::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Held::Keys(keys)) = self.dictation.as_ref().and_then(|d| d.held.clone()) {
            let wanted = keys.modifiers;
            let lost = (wanted.platform && !now.platform)
                || (wanted.control && !now.control)
                || (wanted.alt && !now.alt)
                || (wanted.shift && !now.shift)
                || (wanted.function && !now.function);
            if lost {
                self.let_go(window, cx);
            }
        }
    }

    /// The mouse let go anywhere in the window: the end of a hold on the
    /// microphone button.
    pub(super) fn dictation_mouse_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.dictation.as_ref().and_then(|d| d.held.as_ref()),
            Some(Held::Pointer)
        ) {
            self.let_go(window, cx);
        }
    }

    /// What was held is let go: the recording finishes (or, if the
    /// microphone hadn't started yet, is dropped).
    fn let_go(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.dictation.as_mut() else {
            return;
        };
        d.held = None;
        match d.phase {
            Phase::Recording(_) => self.finish_dictation(window, cx),
            Phase::Starting => {
                self.cancel_dictation(cx);
            }
            _ => {}
        }
    }

    /// The microphone button or the shortcut.
    pub(super) fn toggle_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.dictation.as_ref().map(|d| &d.phase) {
            Some(Phase::Recording(_)) => self.finish_dictation(window, cx),
            Some(Phase::Starting) => {
                self.cancel_dictation(cx);
            }
            // Already on its way.
            Some(Phase::Transcribing | Phase::Tidying(_)) => {}
            None => self.start_dictation(window, cx),
        }
    }

    fn start_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.store.read(cx).selected.clone() else {
            return;
        };
        let Some(composer) = self.composers.get(&thread_id).cloned() else {
            return;
        };
        let voice = self.store.read(cx).voice.clone();
        // A window on another computer dictates with that computer's
        // models, which this one's settings don't show.
        let here = self.store.read(cx).remote().is_none();
        if here && voice.model.trim().is_empty() {
            self.store.update(cx, |s, cx| {
                s.toast_error("Choose a speech model to dictate with.".into(), cx)
            });
            crate::settings::open(
                self.store.clone(),
                Some(crate::settings::Section::Voice),
                cx,
            );
            return;
        }
        let start = {
            let focused =
                gpui_kit::Focusable::focus_handle(composer.read(cx), cx).is_focused(window);
            let c = composer.read(cx);
            let value = c.value().to_string();
            // Not typing in it: the text goes at the end of the draft, not
            // where a cursor last was.
            let range = if focused {
                c.selected_range()
            } else {
                value.len()..value.len()
            };
            (value, range)
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let started = Recorder::start().await;
            let _ = this.update_in(cx, |this, window, cx| match started {
                Ok(recorder) => {
                    let starting = this
                        .dictation
                        .as_ref()
                        .is_some_and(|d| matches!(d.phase, Phase::Starting));
                    if starting {
                        let task = this.tick(window, cx);
                        if let Some(d) = this.dictation.as_mut() {
                            d.phase = Phase::Recording(recorder);
                            d._task = task;
                        }
                        this.ask_live(cx);
                        cx.notify();
                    }
                }
                Err(err) => {
                    this.dictation = None;
                    this.store
                        .update(cx, |s, cx| s.toast_error(format!("{err:#}"), cx));
                    cx.notify();
                }
            });
        });
        self.dictation = Some(Dictation {
            thread_id,
            phase: Phase::Starting,
            start,
            levels: Vec::new(),
            live: Live::Asking,
            heard: Default::default(),
            held: None,
            _task: task,
        });
        cx.notify();
    }

    /// Redraws the meter while recording; finishes when the recorder
    /// stopped by itself (five minutes, the microphone unplugged).
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let go_on = this
                    .update_in(cx, |this, window, cx| {
                        let Some(d) = this.dictation.as_mut() else {
                            return false;
                        };
                        let Phase::Recording(recorder) = &d.phase else {
                            return false;
                        };
                        if let Some(why) = recorder.ended() {
                            this.store.update(cx, |s, cx| s.toast_error(why, cx));
                            this.finish_dictation(window, cx);
                            return false;
                        }
                        d.levels.push(recorder.level());
                        if d.levels.len() > LEVEL_BARS {
                            d.levels.remove(0);
                        }
                        this.send_live_piece(cx);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !go_on {
                    break;
                }
            }
        })
    }

    /// Esc, or the ✕ by the meter: the recording is thrown away.
    pub(super) fn cancel_dictation(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(d) = self.dictation.take() else {
            return false;
        };
        if let Live::On { id, .. } = d.live
            && let Some(client) = self.store.read(cx).client()
        {
            let cancel = client.request(Request::VoiceLiveCancel { id });
            cx.background_executor()
                .spawn(async move {
                    let _ = cancel.await;
                })
                .detach();
        }
        cx.notify();
        true
    }

    /// Ask the daemon to transcribe as the recording comes, if its model
    /// can.
    fn ask_live(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            if let Some(d) = self.dictation.as_mut() {
                d.live = Live::Off;
            }
            return;
        };
        let start = client.request(Request::VoiceLiveStart { language: None });
        cx.spawn(async move |this, cx| {
            let answer = start.await;
            let id = match answer {
                Ok(Response::VoiceLive { id: Some(id), .. }) => Some(id),
                Ok(_) => None,
                Err(err) => {
                    tracing::info!("no live transcription: {}", err.message);
                    None
                }
            };
            let _ = this.update(cx, |this, cx| {
                let recording = this.dictation.as_ref().is_some_and(|d| {
                    d.live == Live::Asking && matches!(d.phase, Phase::Recording(_))
                });
                if !recording {
                    // Finished (or cancelled) meanwhile: it went all at once.
                    if let Some(id) = id
                        && let Some(client) = this.store.read(cx).client()
                    {
                        let cancel = client.request(Request::VoiceLiveCancel { id });
                        cx.background_executor()
                            .spawn(async move {
                                let _ = cancel.await;
                            })
                            .detach();
                    }
                    return;
                }
                if let Some(d) = this.dictation.as_mut() {
                    d.live = match id {
                        Some(id) => Live::On {
                            id,
                            sent: 0,
                            sending: false,
                        },
                        None => Live::Off,
                    };
                }
                this.send_live_piece(cx);
            });
        })
        .detach();
    }

    /// Send what was recorded since the last piece, when nothing is on its
    /// way and there is enough of it.
    fn send_live_piece(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.dictation.as_mut() else {
            return;
        };
        let (Live::On { id, sent, sending }, Phase::Recording(recorder)) = (d.live, &d.phase)
        else {
            return;
        };
        if sending {
            return;
        }
        let piece = recorder.samples_from(sent);
        if piece.len() < LIVE_PIECE {
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        d.live = Live::On {
            id,
            sent: sent + piece.len(),
            sending: true,
        };
        let audio: Vec<u8> = piece.iter().flat_map(|s| s.to_le_bytes()).collect();
        let answer = client.request(Request::VoiceLiveFeed {
            id,
            audio: Bytes(audio),
        });
        cx.spawn(async move |this, cx| {
            let answer = answer.await;
            let _ = this.update(cx, |this, cx| {
                let Some(d) = this.dictation.as_mut() else {
                    return;
                };
                let Live::On { id: now, sent, .. } = d.live else {
                    return;
                };
                if now != id {
                    return;
                }
                match answer {
                    Ok(Response::VoiceLive {
                        committed,
                        tentative,
                        ..
                    }) => {
                        d.heard = (committed, tentative);
                        d.live = Live::On {
                            id,
                            sent,
                            sending: false,
                        };
                    }
                    other => {
                        // The whole recording goes at the end instead.
                        if let Err(err) = other {
                            tracing::warn!("live transcription stopped: {}", err.message);
                        }
                        d.live = Live::Off;
                        d.heard = Default::default();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.dictation.as_mut() else {
            return;
        };
        let Phase::Recording(_) = d.phase else {
            return;
        };
        let Phase::Recording(recorder) = std::mem::replace(&mut d.phase, Phase::Transcribing)
        else {
            unreachable!()
        };
        let pcm = recorder.finish();
        if (pcm.len() as f32) < MIN_SECONDS * SAMPLE_RATE as f32 {
            self.dictation = None;
            cx.notify();
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            self.dictation = None;
            self.store.update(cx, |s, cx| {
                s.toast_error("Not connected to Agentloft.".into(), cx)
            });
            return;
        };
        let audio: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let cleanup = {
            let voice = &self.store.read(cx).voice;
            // On another computer its own settings decide; the daemon
            // answers the transcript as heard when cleanup is off there.
            self.store.read(cx).remote().is_some() || !matches!(voice.cleanup.trim(), "" | "off")
        };
        let live = match d.live {
            Live::On { id, .. } => Some(id),
            _ => None,
        };
        d._task = cx.spawn_in(window, async move |this, cx| {
            let heard = match live {
                Some(id) => {
                    // The piece on its way goes first.
                    let sent = loop {
                        let state = this
                            .read_with(cx, |this, _| this.dictation.as_ref().map(|d| d.live))
                            .ok()
                            .flatten();
                        match state {
                            Some(Live::On { sending: true, .. }) => {
                                cx.background_executor()
                                    .timer(Duration::from_millis(20))
                                    .await;
                            }
                            Some(Live::On { sent, .. }) => break Some(sent),
                            Some(_) => break None,
                            None => return,
                        }
                    };
                    match sent {
                        Some(sent) => {
                            let rest = audio.get(sent * 2..).unwrap_or_default().to_vec();
                            match client
                                .request(Request::VoiceLiveFinish {
                                    id,
                                    audio: Bytes(rest),
                                })
                                .await
                            {
                                Ok(answer) => Ok(answer),
                                Err(err) => {
                                    tracing::warn!("live transcription failed: {}", err.message);
                                    whole(client.clone(), audio).await
                                }
                            }
                        }
                        // It failed while recording.
                        None => whole(client.clone(), audio).await,
                    }
                }
                None => whole(client.clone(), audio).await,
            };
            let text = match heard {
                Ok(Response::Text { text }) => text,
                Ok(_) => String::new(),
                Err(err) => {
                    let _ = this.update(cx, |this, cx| {
                        this.dictation = None;
                        this.store
                            .update(cx, |s, cx| s.toast_error(err.message, cx));
                        cx.notify();
                    });
                    return;
                }
            };
            if text.trim().is_empty() {
                let _ = this.update(cx, |this, cx| {
                    this.dictation = None;
                    this.store
                        .update(cx, |s, cx| s.toast_error("Nothing was heard.".into(), cx));
                    cx.notify();
                });
                return;
            }
            if !cleanup {
                let _ = this.update_in(cx, |this, window, cx| this.put_in(&text, window, cx));
                return;
            }
            let Ok(Some((before, after))) = this.update(cx, |this, cx| {
                let around = this.around_insertion(cx);
                if let Some(d) = this.dictation.as_mut() {
                    d.phase = Phase::Tidying(text.clone());
                }
                cx.notify();
                around
            }) else {
                return;
            };
            let tidied = client
                .request(Request::VoiceCleanup {
                    transcript: text.clone(),
                    before,
                    after,
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // Skipped meanwhile.
                if !matches!(
                    this.dictation.as_ref().map(|d| &d.phase),
                    Some(Phase::Tidying(_))
                ) {
                    return;
                }
                let text = match tidied {
                    Ok(Response::Text { text: tidied }) if !tidied.trim().is_empty() => tidied,
                    Ok(_) => text,
                    Err(err) => {
                        this.store.update(cx, |s, cx| {
                            s.toast_error(
                                format!("Cleanup failed, so the text is as heard: {}", err.message),
                                cx,
                            )
                        });
                        text
                    }
                };
                this.put_in(&text, window, cx);
            });
        });
        cx.notify();
    }

    /// Skip: the transcript goes in as heard, without waiting for cleanup.
    fn skip_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Phase::Tidying(raw)) = self.dictation.as_ref().map(|d| &d.phase) {
            let raw = raw.clone();
            self.put_in(&raw, window, cx);
        }
    }

    /// The draft before and after where the text will go.
    fn around_insertion(&self, cx: &Context<Self>) -> Option<(String, String)> {
        let d = self.dictation.as_ref()?;
        let composer = self.composers.get(&d.thread_id)?;
        let value = composer.read(cx).value().to_string();
        let range = self.insertion(&value, composer.read(cx).selected_range());
        Some((
            value[..range.start].to_owned(),
            value[range.end..].to_owned(),
        ))
    }

    /// Where the text goes: the selection recording began with, if the
    /// draft is as it was; otherwise where the cursor is now.
    fn insertion(&self, value: &str, now: Range<usize>) -> Range<usize> {
        let fits = |r: &Range<usize>| {
            r.start <= r.end
                && r.end <= value.len()
                && value.is_char_boundary(r.start)
                && value.is_char_boundary(r.end)
        };
        match &self.dictation {
            Some(d) if d.start.0 == value && fits(&d.start.1) => d.start.1.clone(),
            _ if fits(&now) => now,
            _ => value.len()..value.len(),
        }
    }

    /// Put `text` into the draft and end the dictation.
    fn put_in(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.dictation.take() else {
            return;
        };
        cx.notify();
        let Some(composer) = self.composers.get(&d.thread_id).cloned() else {
            return;
        };
        let (value, now) = {
            let c = composer.read(cx);
            (c.value().to_string(), c.selected_range())
        };
        self.dictation = Some(d);
        let range = self.insertion(&value, now);
        self.dictation = None;
        let text = spaced(&value[..range.start], text.trim(), &value[range.end..]);
        // An edit like any other: it can be undone, and the draft follows.
        composer.update(cx, |c, cx| {
            c.set_selected_range(range, cx);
            c.insert(text, window, cx);
        });
        let focus = gpui_kit::Focusable::focus_handle(composer.read(cx), cx);
        window.focus(&focus, cx);
    }

    /// The microphone button, or while dictating, the recording's controls.
    pub(super) fn dictation_controls(
        &self,
        thread_id: &ThreadId,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.store.read(cx).voice.enabled {
            return None;
        }
        let active = self
            .dictation
            .as_ref()
            .filter(|d| &d.thread_id == thread_id);
        let Some(d) = active else {
            let hold = self.store.read(cx).voice.hold_to_talk();
            let what = if hold { "Hold to dictate" } else { "Dictate" };
            let side = crate::voice::keys::SideKey::parse(&self.store.read(cx).voice.key);
            let tip = match (side, crate::keymap::keys_for("dictate", cx)) {
                (Some(side), _) => format!("{what} ({})", side.label()),
                (None, Some(keys)) => format!("{what} ({})", crate::keymap::display(&keys)),
                (None, None) => what.to_owned(),
            };
            return Some(
                div()
                    .id("dictate")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .text_color(p.muted_foreground)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(Icon::new(IconName::Mic).small())
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .claims_press()
                    .when(hold, |d| {
                        d.on_mouse_down(
                            gpui_kit::MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.hold_button(window, cx);
                                cx.stop_propagation();
                            }),
                        )
                    })
                    .when(!hold, |d| {
                        d.on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_dictation(window, cx);
                            cx.stop_propagation();
                        }))
                    })
                    .into_any_element(),
            );
        };
        let row = h_flex().gap_1p5().items_center().text_xs();
        let cancel = div()
            .id("dictate-cancel")
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .text_color(p.muted_foreground)
            .cursor_pointer()
            .hover(|d| d.bg(p.hover).text_color(p.foreground))
            .child(Icon::new(IconName::X).xsmall())
            .claims_press()
            .on_click(cx.listener(|this, _, _, cx| {
                this.cancel_dictation(cx);
                cx.stop_propagation();
            }));
        let element = match &d.phase {
            Phase::Starting => row
                .text_color(p.muted_foreground)
                .child(crate::spinner::Spinner::new().xsmall())
                .child("Starting the microphone…")
                .child(cancel),
            Phase::Recording(recorder) => {
                let secs = recorder.seconds() as u32;
                row.child(cancel)
                    .child(meter(&d.levels, p))
                    .child(div().text_color(p.muted_foreground).child(format!(
                        "{}:{:02}",
                        secs / 60,
                        secs % 60
                    )))
                    .child(
                        div()
                            .id("dictate-done")
                            .size(px(26.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(p.danger)
                            .text_color(gpui_kit::white())
                            .cursor_pointer()
                            .child(Icon::new(IconName::Check).small())
                            .claims_press()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.finish_dictation(window, cx);
                                cx.stop_propagation();
                            })),
                    )
            }
            Phase::Transcribing => row
                .text_color(p.muted_foreground)
                .child(crate::spinner::Spinner::new().xsmall())
                .child("Transcribing…")
                .child(cancel),
            Phase::Tidying(_) => row
                .text_color(p.muted_foreground)
                .child(crate::spinner::Spinner::new().xsmall())
                .child("Tidying…")
                .child(
                    div()
                        .id("dictate-skip")
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(6.))
                        .text_color(p.foreground)
                        .cursor_pointer()
                        .hover(|d| d.bg(p.hover))
                        .child("Skip")
                        .claims_press()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.skip_cleanup(window, cx);
                            cx.stop_propagation();
                        })),
                ),
        };
        Some(
            element
                .id(ElementId::Name("dictation".into()))
                .into_any_element(),
        )
    }
}

impl WorkspaceView {
    /// What live transcription has heard, while recording and until the
    /// text goes into the draft: the part that stays in the text colour,
    /// the part that may still change fainter.
    pub(super) fn dictation_preview(
        &self,
        thread_id: &ThreadId,
        p: &Palette,
    ) -> Option<AnyElement> {
        let d = self
            .dictation
            .as_ref()
            .filter(|d| &d.thread_id == thread_id)?;
        let (committed, tentative) = &d.heard;
        if committed.trim().is_empty() && tentative.trim().is_empty() {
            return None;
        }
        let text = format!("{committed}{tentative}");
        let still = committed.len()..text.len();
        Some(
            div()
                .px_2()
                .text_sm()
                .text_color(p.foreground)
                .child(
                    gpui_kit::StyledText::new(text).with_highlights((!still.is_empty()).then(
                        || {
                            (
                                still,
                                gpui_kit::HighlightStyle {
                                    color: Some(p.muted_foreground),
                                    ..Default::default()
                                },
                            )
                        },
                    )),
                )
                .into_any_element(),
        )
    }
}

/// The whole recording, transcribed at once.
async fn whole(
    client: loft_client::Client,
    audio: Vec<u8>,
) -> Result<Response, loft_protocol::ErrorBody> {
    client
        .request(Request::VoiceTranscribe {
            audio: Bytes(audio),
            language: None,
            model: None,
        })
        .await
}

/// Bars in the level meter.
const LEVEL_BARS: usize = 24;

fn meter(levels: &[f32], p: &Palette) -> impl gpui_kit::IntoElement {
    let pad = LEVEL_BARS.saturating_sub(levels.len());
    h_flex().h(px(18.)).gap(px(2.)).items_center().children(
        std::iter::repeat_n(0.0, pad)
            .chain(levels.iter().copied())
            .map(|level: f32| {
                let h = 3. + 15. * level.clamp(0., 1.);
                div().w(px(2.)).h(px(h)).rounded_full().bg(p.accent)
            }),
    )
}

/// `text` with a space before it when it would otherwise run into the word
/// before, and after it when the next character is a word's.
fn spaced(before: &str, text: &str, after: &str) -> String {
    let mut out = String::new();
    if before.chars().last().is_some_and(|c| !c.is_whitespace()) {
        out.push(' ');
    }
    out.push_str(text);
    if after.chars().next().is_some_and(char::is_alphanumeric) {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::spaced;

    #[test]
    fn dictated_text_is_spaced_from_its_neighbours() {
        assert_eq!(spaced("", "Hello.", ""), "Hello.");
        assert_eq!(spaced("Then", "run it", ""), " run it");
        assert_eq!(spaced("Then ", "run it", "now"), "run it ");
        assert_eq!(spaced("a\n", "b", ". c"), "b");
    }
}

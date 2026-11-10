//! Settings → Web access (ADR-0016): turn Agentloft-in-a-browser on, pair a
//! browser with the link, see who is signed in and sign them out, and choose
//! where the daemon listens. Everything here is written to `[web]` in
//! config.toml; the daemon follows it and reports back what it is doing.
//!
//! Desktop only: a browser session may not manage its own access.

use super::{Field, SettingsView, row, text_input};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, ClipboardItem, Context, FontWeight, IntoElement, ParentElement as _, SharedString,
    Styled as _, Task, div, prelude::FluentBuilder as _, px,
};
use loft_config::SettingValue;
use loft_protocol::{Request, Response, WebSession, WebStatus};
use std::time::Duration;

#[derive(Default)]
pub(super) struct WebPage {
    /// What the daemon last reported; `None` until it has answered.
    status: Option<WebStatus>,
    /// The pairing link is shown in full (it is a secret, so not by default).
    revealed: bool,
    /// "Copied" shows on the copy button until this passes.
    copied_until: Option<web_time::Instant>,
    /// Re-asks the daemon while the page is open.
    poll: Option<Task<()>>,
}

pub(super) fn heading(title: &str, detail: &str, p: &Palette) -> AnyElement {
    v_flex()
        .pt_6()
        .pb_1()
        .gap_0p5()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .when(!detail.is_empty(), |d| {
            d.child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(detail.to_owned()),
            )
        })
        .into_any_element()
}

/// A coloured note across the page.
fn banner(icon: IconName, text: String, danger: bool, p: &Palette) -> AnyElement {
    let (bg, fg) = if danger {
        (p.danger_soft, p.danger)
    } else {
        (p.warning_soft, p.warning)
    };
    h_flex()
        .mt_3()
        .px_3()
        .py_2()
        .gap_2()
        .items_start()
        .rounded(px(8.))
        .bg(bg)
        .text_sm()
        .text_color(fg)
        .child(Icon::new(icon).small().mt(px(2.)))
        .child(div().flex_1().min_w_0().child(text))
        .into_any_element()
}

/// "just now", "5 min ago", "3 h ago", "2 d ago".
fn ago(ms: i64) -> String {
    let now = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    match ((now - ms) / 1000).max(0) {
        0..=59 => "just now".into(),
        s @ 60..=3599 => format!("{} min ago", s / 60),
        s @ 3600..=86_399 => format!("{} h ago", s / 3600),
        s => format!("{} d ago", s / 86_400),
    }
}

/// The pairing link with its token hidden.
fn masked(link: &str) -> String {
    match link.split_once("#token=") {
        Some((base, _)) => format!("{base}#token=••••••••"),
        None => link.to_owned(),
    }
}

impl SettingsView {
    /// Ask the daemon how web access stands.
    pub(super) fn refresh_web(&mut self, cx: &mut Context<Self>) {
        self.web_request(Request::WebStatus, cx);
    }

    /// Send a web-access request; they all answer with the new status.
    fn web_request(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let pending = client.request(request);
        cx.spawn(async move |this, cx| {
            let result = pending.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Response::WebStatus { status }) => this.web.status = Some(status),
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Keep the status fresh while the page is showing (sessions come and
    /// go without anything changing here).
    fn poll_web(&mut self, cx: &mut Context<Self>) {
        if self.web.poll.is_some() {
            return;
        }
        self.refresh_web(cx);
        self.web.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let showing = this.update(cx, |this, cx| {
                    let showing = this.section == super::Section::Web;
                    if showing {
                        this.refresh_web(cx);
                    } else {
                        this.web.poll = None;
                    }
                    showing
                });
                if !matches!(showing, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn set_web(&mut self, key: &'static str, value: SettingValue, cx: &mut Context<Self>) {
        self.write(&["web", key], value, true, cx);
    }

    pub(super) fn web_page(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        self.poll_web(cx);
        let web = self.config.web.clone();
        let status = self.web.status.clone().unwrap_or_default();
        let plan = web.plan();

        // -- on/off and how it stands ---------------------------------------
        let state: (String, gpui_kit::Hsla) = if !web.enabled {
            ("Off".into(), p.muted_foreground)
        } else if let Some(error) = &status.error {
            (format!("Not running: {error}"), p.danger)
        } else if let Err(error) = &plan {
            (format!("Not running: {error}"), p.danger)
        } else if status.running {
            (
                format!("On at {}", status.url.clone().unwrap_or_default()),
                p.success,
            )
        } else {
            ("Starting…".into(), p.muted_foreground)
        };
        let mut page = v_flex().child(row(
            "Use Agentloft in a browser",
            "Serves Agentloft from this computer. Off until you turn it on.",
            Switch::new("web-enabled")
                .checked(web.enabled)
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    let value = if *on {
                        SettingValue::Bool(true)
                    } else {
                        SettingValue::Unset
                    };
                    this.set_web("enabled", value, cx);
                }))
                .into_any_element(),
            p,
        ));
        page = page.child(
            h_flex()
                .pt_3()
                .gap_2()
                .items_center()
                .text_sm()
                .text_color(state.1)
                .child(div().size(px(7.)).rounded_full().bg(state.1))
                .child(div().flex_1().min_w_0().child(state.0)),
        );
        if web.enabled && status.running && !status.bundle {
            page = page.child(banner(
                IconName::TriangleAlert,
                "The web client's files weren't found, so browsers get an empty page. Build them with `cargo xtask web`.".into(),
                false,
                p,
            ));
        }

        // -- anything but this computer is said loudly -----------------------
        let remote = status.running && status.remote
            || plan
                .as_ref()
                .is_ok_and(|plan| !plan.addr.ip().is_loopback());
        if web.enabled && remote {
            let insecure = status.insecure_remote || plan.as_ref().is_ok_and(|p| p.insecure_remote);
            page = page.child(banner(
                IconName::TriangleAlert,
                if insecure {
                    "Other devices on the network can reach Agentloft, and the connection is not encrypted: anyone on that network can read it, pairing link included. Use this only on a network that is itself private, like a VPN.".into()
                } else {
                    "Other devices on the network can reach Agentloft. Anyone who gets the pairing link can run commands on this computer.".into()
                },
                insecure,
                p,
            ));
        }

        // -- pairing ----------------------------------------------------------
        if let Some(link) = status.pairing_url.clone().filter(|_| status.running) {
            let copied = self
                .web
                .copied_until
                .is_some_and(|until| web_time::Instant::now() < until);
            let shown = if self.web.revealed {
                link.clone()
            } else {
                masked(&link)
            };
            let (copy_link, open_link) = (link.clone(), link.clone());
            page = page
                .child(heading(
                    "Pairing link",
                    "Open it in the browser you want to use. It signs that browser in, so treat it like a password.",
                    p,
                ))
                .child(
                    div()
                        .mt_2()
                        .px_3()
                        .py_2()
                        .rounded(px(8.))
                        .bg(p.muted)
                        .text_xs()
                        .font_family(p.mono.clone())
                        .text_color(p.muted_foreground)
                        .overflow_hidden()
                        .child(shown),
                )
                .child(
                    h_flex()
                        .pt_2()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new("web-copy")
                                .small()
                                .primary()
                                .icon(Icon::new(if copied {
                                    IconName::Check
                                } else {
                                    IconName::Copy
                                }))
                                .label(if copied { "Copied" } else { "Copy link" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_link.clone(),
                                    ));
                                    this.web.copied_until = Some(
                                        web_time::Instant::now() + Duration::from_secs(2),
                                    );
                                    cx.notify();
                                    cx.spawn(async move |this, cx| {
                                        cx.background_executor()
                                            .timer(Duration::from_millis(2100))
                                            .await;
                                        let _ = this.update(cx, |_, cx| cx.notify());
                                    })
                                    .detach();
                                })),
                        )
                        .child(
                            Button::new("web-open")
                                .small()
                                .outline()
                                .icon(Icon::new(IconName::ExternalLink))
                                .label("Open in browser")
                                .on_click(move |_, _, cx| cx.open_url(&open_link)),
                        )
                        .child(
                            Button::new("web-reveal")
                                .small()
                                .ghost()
                                .icon(Icon::new(IconName::Eye))
                                .label(if self.web.revealed { "Hide" } else { "Show" })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.web.revealed = !this.web.revealed;
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("web-rotate")
                                .small()
                                .ghost()
                                .icon(Icon::new(IconName::RefreshCw))
                                .label("New link")
                                .tooltip("Replaces the link and signs every browser out")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.web.revealed = false;
                                    this.web_request(Request::WebRotateToken, cx);
                                })),
                        ),
                );
        }

        // -- who is signed in -------------------------------------------------
        if web.enabled && (status.running || !status.sessions.is_empty()) {
            page = page.child(
                h_flex()
                    .items_end()
                    .child(div().flex_1().child(heading(
                        "Signed-in browsers",
                        "Each stays signed in until it expires or you sign it out.",
                        p,
                    )))
                    .when(status.sessions.len() > 1, |d| {
                        d.child(
                            Button::new("web-revoke-all")
                                .small()
                                .ghost()
                                .label("Sign out all")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.web_request(Request::WebRevokeAllSessions, cx);
                                })),
                        )
                    }),
            );
            if status.sessions.is_empty() {
                page = page.child(
                    div()
                        .py_3()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("None yet."),
                );
            }
            for session in &status.sessions {
                page = page.child(self.session_row(session, p, cx));
            }
        }

        // -- what a browser may do ----------------------------------------------
        // Unset follows who can reach the listener (on for this computer
        // only); a choice that matches that is left unset, so opening the
        // listener up later doesn't carry a shell along with it.
        let auto_terminal = plan.as_ref().is_ok_and(|plan| plan.local_only());
        let terminal = web.allow_terminal.unwrap_or(auto_terminal);
        page = page
            .child(heading(
                "In the browser",
                "What a signed-in browser may do beyond the conversation.",
                p,
            ))
            .child(row(
                "Terminals",
                if auto_terminal {
                    "A shell on this computer, in the browser. On while only this computer can reach Agentloft."
                } else {
                    "A shell on this computer, in the browser. Off unless you turn it on, because other devices can reach Agentloft."
                },
                Switch::new("web-terminal")
                    .checked(terminal)
                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                        let value = if *on == auto_terminal {
                            SettingValue::Unset
                        } else {
                            SettingValue::Bool(*on)
                        };
                        this.set_web("allow_terminal", value, cx);
                    }))
                    .into_any_element(),
                p,
            ))
            .child(row(
                "Browser",
                if auto_terminal {
                    "Web pages loaded on this computer and shown in the browser. On while only this computer can reach Agentloft."
                } else {
                    "Web pages loaded on this computer and shown in the browser, with its network and its logins. Off unless you turn it on, because other devices can reach Agentloft."
                },
                Switch::new("web-browser")
                    .checked(web.allow_browser.unwrap_or(auto_terminal))
                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                        let value = if *on == auto_terminal {
                            SettingValue::Unset
                        } else {
                            SettingValue::Bool(*on)
                        };
                        this.set_web("allow_browser", value, cx);
                    }))
                    .into_any_element(),
                p,
            ));

        // -- where it listens ---------------------------------------------------
        page = page
            .child(heading(
                "Where it listens",
                "Changes apply at once and sign nobody out.",
                p,
            ))
            .child(row(
                "Address",
                "127.0.0.1 keeps it to this computer. Any other address lets other devices connect, and needs a certificate.",
                text_input(&self.input(Field::WebBind)),
                p,
            ))
            .child(row(
                "Port",
                "The link's port.",
                text_input(&self.input(Field::WebPort)),
                p,
            ))
            .child(row(
                "Certificate",
                "A PEM certificate chain, to serve HTTPS.",
                text_input(&self.input(Field::WebCert)),
                p,
            ))
            .child(row(
                "Private key",
                "The certificate's PEM private key.",
                text_input(&self.input(Field::WebKey)),
                p,
            ))
            .child(row(
                "Other names",
                "Names a proxy or tunnel reaches this computer by, separated by commas. Requests for any other name are refused.",
                text_input(&self.input(Field::WebHosts)),
                p,
            ))
            .child(row(
                "Allow without a certificate",
                "Serve plain HTTP to other devices. Only for a network that is already private, like a VPN.",
                Switch::new("web-insecure")
                    .checked(web.allow_insecure_remote)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        let value = if *on {
                            SettingValue::Bool(true)
                        } else {
                            SettingValue::Unset
                        };
                        this.set_web("allow_insecure_remote", value, cx);
                    }))
                    .into_any_element(),
                p,
            ));
        page.into_any_element()
    }

    fn session_row(&self, session: &WebSession, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let id = session.id.clone();
        let connected = session.connections > 0;
        let detail = format!(
            "{} · {} · signed in {}",
            session.addr,
            if connected {
                "connected now".to_owned()
            } else {
                format!("last seen {}", ago(session.last_seen_ms))
            },
            ago(session.created_ms),
        );
        h_flex()
            .py_2p5()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(p.border.opacity(0.6))
            .child(
                Icon::new(IconName::MonitorSmartphone)
                    .small()
                    .text_color(if connected {
                        p.success
                    } else {
                        p.muted_foreground
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().text_sm().child(session.label.clone()))
                    .child(div().text_xs().text_color(p.muted_foreground).child(detail)),
            )
            .child(
                Button::new(SharedString::from(format!("web-revoke-{}", session.id)))
                    .small()
                    .outline()
                    .label("Sign out")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.web_request(Request::WebRevokeSession { id: id.clone() }, cx);
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_is_masked_until_revealed() {
        let link = "http://127.0.0.1:4780/#token=abcDEF123";
        let shown = masked(link);
        assert!(!shown.contains("abcDEF123"));
        assert!(shown.starts_with("http://127.0.0.1:4780/#token="));
        assert_eq!(masked("http://127.0.0.1:4780"), "http://127.0.0.1:4780");
    }
}

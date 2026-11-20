//! Settings → Web access (ADR-0016): turn Agentloft-in-a-browser on, pair a
//! browser with the link, see who is signed in and sign them out, and choose
//! where the daemon listens; put it on the internet with Cloudflare Tunnel
//! and link this computer to the website, to open it from there (ADR-0019).
//! Everything here is written to `[web]` in config.toml; the daemon follows
//! it and reports back what it is doing.
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
use loft_protocol::{Request, Response, WebAddress, WebSession, WebStatus, WebsiteStatus};
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
                .child(super::selectable(title.to_owned())),
        )
        .when(!detail.is_empty(), |d| {
            d.child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(super::selectable(detail.to_owned())),
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

/// `text` as a QR code: an SVG of black squares on white, with the quiet
/// zone round it that scanners need.
fn qr_svg(text: &str) -> Option<Vec<u8>> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let quiet = 4;
    let size = width + 2 * quiet;
    let mut path = String::new();
    for (ix, color) in code.to_colors().into_iter().enumerate() {
        if color == qrcode::Color::Dark {
            let (x, y) = (ix % width + quiet, ix / width + quiet);
            path.push_str(&format!("M{x} {y}h1v1h-1z"));
        }
    }
    Some(
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" \
             shape-rendering=\"crispEdges\"><rect width=\"{size}\" height=\"{size}\" \
             fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>"
        )
        .into_bytes(),
    )
}

/// What an address is, in words.
fn address_label(kind: &str) -> &'static str {
    match kind {
        "local" => "This computer",
        "lan" => "Local network",
        "cloudflare" => "Public (Cloudflare)",
        _ => "Name",
    }
}

/// `https://agentloft.example.com` as `agentloft.example.com`.
fn bare_host(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

/// A line of small text in `color`, for a status under a row.
fn note(text: String, color: gpui_kit::Hsla) -> AnyElement {
    div()
        .pt_2()
        .text_xs()
        .text_color(color)
        .child(text)
        .into_any_element()
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

    /// Start linking this computer to the website, then open its page in
    /// the browser (it asks to sign in, then to approve).
    fn link_start(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let pending = client.request(Request::WebLinkStart);
        cx.spawn(async move |this, cx| {
            let result = pending.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Response::WebStatus { status }) => {
                        if let Some(link) = &status.website.pending {
                            open_secret_link(cx, &link.verify_url);
                        }
                        this.web.status = Some(status);
                    }
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

        // -- network access: other devices on the local network ------------
        let default_url = status
            .addresses
            .iter()
            .find(|a| a.default && a.kind != "local")
            .map(|a| a.url.clone());
        page = page.child(row(
            "Network access",
            &match (&default_url, web.network && status.running) {
                (Some(url), true) => format!("Reachable at {url}/"),
                _ => "Let your phone or another computer on your local network open Agentloft. \
                      Connections from anywhere else are refused."
                    .to_owned(),
            },
            Switch::new("web-network")
                .checked(web.network)
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    let value = if *on {
                        SettingValue::Bool(true)
                    } else {
                        SettingValue::Unset
                    };
                    // Turning it on turns web access on too.
                    if *on && !this.config.web.enabled {
                        this.set_web("enabled", SettingValue::Bool(true), cx);
                    }
                    this.set_web("network", value, cx);
                }))
                .into_any_element(),
            p,
        ));
        page = page.child(self.tunnel_row(&web, &status, p, cx));
        if web.enabled && status.running && status.addresses.len() > 1 {
            for address in &status.addresses {
                page = page.child(self.address_row(address, p, cx));
            }
        }
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
                if web.tunnel.is_some() && status.tunnel.as_ref().is_some_and(|t| t.state == "on") {
                    "Agentloft is on the internet. Only a browser signed in through the website with your Google account gets in there; pairing links don't work on that address. Whoever controls your Google account, the website or Cloudflare could sign in.".into()
                } else if status.network {
                    "Devices on your local network can reach Agentloft, over plain HTTP: anyone on that network could read it, pairing link included. Turn this off on networks you don't trust.".into()
                } else if insecure {
                    "Other devices on the network can reach Agentloft, and the connection is not encrypted: anyone on that network can read it, pairing link included. Use this only on a network that is itself private, like a VPN.".into()
                } else {
                    "Other devices on the network can reach Agentloft. Anyone who gets the pairing link can run commands on this computer.".into()
                },
                insecure,
                p,
            ));
        }

        // -- the website ----------------------------------------------------------
        page = page.child(self.website_section(&web, &status, p, cx));

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
                // For a phone: point its camera at the link (hidden with
                // it, being the same secret).
                .when(status.remote && self.web.revealed, |d| {
                    d.children(qr_svg(&link).map(|svg| {
                        h_flex()
                            .pt_3()
                            .gap_3()
                            .items_center()
                            .child(
                                gpui_kit::img(std::sync::Arc::new(gpui_kit::Image::from_bytes(
                                    gpui_kit::ImageFormat::Svg,
                                    svg,
                                )))
                                .size(px(168.))
                                .rounded(px(8.)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(p.muted_foreground)
                                    .child("Scan it with your phone's camera to open Agentloft there and sign it in."),
                            )
                    }))
                })
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
                                .on_click(move |_, _, cx| open_secret_link(cx, &open_link)),
                        )
                        .child(
                            Button::new("web-reveal")
                                .small()
                                .ghost()
                                .icon(Icon::new(IconName::Eye))
                                .label(match (self.web.revealed, status.remote) {
                                    (true, _) => "Hide",
                                    (false, true) => "Show with QR code",
                                    (false, false) => "Show",
                                })
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
                if web.network {
                    "Network access listens on every address, so this one isn't used while it is on."
                } else {
                    "127.0.0.1 keeps it to this computer. Any other address lets other devices connect, and needs a certificate."
                },
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

    /// Public with Cloudflare Tunnel (`[web] tunnel`): this computer's own
    /// name on the internet, and how the tunnel stands.
    fn tunnel_row(
        &self,
        web: &loft_config::WebConfig,
        status: &WebStatus,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on = web.tunnel.is_some();
        let tunnel = status.tunnel.clone().filter(|_| on);
        let detail = match &tunnel {
            Some(t) if t.state == "on" && t.url.is_some() => format!(
                "On the internet at {}/ through Cloudflare. Only a browser signed in on the website gets in.",
                t.url.clone().unwrap_or_default()
            ),
            Some(t) if t.state == "on" || t.state == "starting" => {
                "Connecting to Cloudflare…".to_owned()
            }
            _ if on && !web.enabled => "Turn on Use Agentloft in a browser first.".to_owned(),
            _ => "Open Agentloft from your phone anywhere, through the website below. Nothing to install or sign up for: Agentloft fetches Cloudflare's cloudflared if this computer doesn't have it. Only a browser signed in on the website with your Google account gets in; pairing links don't work there.".to_owned(),
        };
        let mut block = v_flex().child(row(
            "Public through Cloudflare",
            &detail,
            Switch::new("web-tunnel")
                .checked(on)
                .on_click(cx.listener(|this, on: &bool, _, cx| {
                    if *on && !this.config.web.enabled {
                        this.set_web("enabled", SettingValue::Bool(true), cx);
                    }
                    let value = if *on {
                        SettingValue::Str(loft_config::WebConfig::QUICK_TUNNEL.into())
                    } else {
                        SettingValue::Unset
                    };
                    this.set_web("tunnel", value, cx);
                }))
                .into_any_element(),
            p,
        ));
        if on {
            block = block.child(row(
                "Your own domain (optional)",
                "Empty: a free address from Cloudflare, new each time it starts (the website keeps up). Or a name on a domain in your Cloudflare account, with cloudflared logged in to it (cloudflared tunnel login).",
                text_input(&self.input(Field::WebTunnel)),
                p,
            ));
        }
        let waiting = tunnel.as_ref().is_some_and(|t| t.state == "waiting");
        if let Some(why) = tunnel
            .as_ref()
            .and_then(|t| t.error.clone())
            .filter(|_| waiting)
        {
            // A next step (link the computer), not a failure.
            block = block.child(note(why, p.muted_foreground));
        } else if let Some(error) = tunnel.and_then(|t| t.error) {
            block = block.child(
                v_flex().child(note(error, p.danger)).child(
                    h_flex().pt_2().gap_2().child(
                        Button::new("web-tunnel-retry")
                            .small()
                            .ghost()
                            .icon(Icon::new(IconName::RefreshCw))
                            .label("Try again")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.web_request(Request::WebSync, cx);
                            })),
                    ),
                ),
            );
        }
        block.into_any_element()
    }

    /// Link this computer to the website, to open it from there.
    fn website_section(
        &self,
        web: &loft_config::WebConfig,
        status: &WebStatus,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let site: WebsiteStatus = status.website.clone();
        let host = bare_host(&site.url).to_owned();
        let mut section = v_flex().child(heading(
            "Open from anywhere",
            &format!(
                "Sign in with Google at {host} to see this computer and open it from your phone or any other browser. Agentloft's traffic doesn't go through the website; it only signs you in."
            ),
            p,
        ));
        if let Some(account) = site.account.clone() {
            let open_site = site.url.clone();
            let detail = match &site.reported_url {
                Some(url) => format!("{host} opens it at {url}/."),
                None if web.tunnel.is_some() => {
                    format!("{host} learns its address once the tunnel is connected.")
                }
                None => format!(
                    "{host} has no address to open it at: turn on Public through Cloudflare above."
                ),
            };
            section = section.child(row(
                &format!("Linked to {account}"),
                &detail,
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("web-site-open")
                            .small()
                            .outline()
                            .icon(Icon::new(IconName::ExternalLink))
                            .label("Open website")
                            .on_click(move |_, _, cx| cx.open_url(&open_site)),
                    )
                    .child(
                        Button::new("web-site-unlink")
                            .small()
                            .ghost()
                            .label("Unlink")
                            .tooltip("Removes this computer from the website")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.web_request(Request::WebUnlink, cx);
                            })),
                    )
                    .into_any_element(),
                p,
            ));
        } else if let Some(pending) = site.pending.clone() {
            let again = pending.verify_url.clone();
            // The code is what the person compares with the website's page,
            // so it is shown large, not inside a sentence.
            let buttons = h_flex()
                .gap_2()
                .child(
                    Button::new("web-site-again")
                        .small()
                        .outline()
                        .icon(Icon::new(IconName::ExternalLink))
                        .label("Open the page again")
                        .on_click(move |_, _, cx| open_secret_link(cx, &again)),
                )
                .child(
                    Button::new("web-site-cancel")
                        .small()
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.web_request(Request::WebLinkCancel, cx);
                        })),
                );
            section = section.child(
                h_flex()
                    .gap_6()
                    .items_center()
                    .py_3()
                    .border_b_1()
                    .border_color(p.border.opacity(0.6))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Waiting for you in the browser"),
                            )
                            .child(div().text_xs().text_color(p.muted_foreground).child(
                                "Sign in there, and approve only if the page shows this code:",
                            ))
                            .child(
                                h_flex().pt_2().child(
                                    div()
                                        .px_4()
                                        .py_1p5()
                                        .rounded(px(8.))
                                        .border_1()
                                        .border_color(p.border)
                                        .bg(p.muted)
                                        .font_family(p.mono.clone())
                                        .text_size(px(26.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(p.foreground)
                                        .child(pending.user_code.clone()),
                                ),
                            ),
                    )
                    .child(div().flex_shrink_0().child(buttons)),
            );
        } else {
            section = section.child(row(
                "Not linked",
                &format!("Opens {host} in your browser, to sign in with Google and approve. Linking also puts this computer online through Cloudflare (Public through Cloudflare, above), reachable only through the website."),
                Button::new("web-site-link")
                    .small()
                    .primary()
                    .label("Link this computer")
                    .on_click(cx.listener(|this, _, _, cx| this.link_start(cx)))
                    .into_any_element(),
                p,
            ));
        }
        if let Some(error) = site.error.clone() {
            section = section.child(note(error, p.danger));
        }
        section.into_any_element()
    }

    /// One place web access can be reached: what it is, its address, and
    /// whether the pairing link uses it.
    fn address_row(&self, address: &WebAddress, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let host = address.host.clone();
        h_flex()
            .py_2p5()
            .pl_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(p.border.opacity(0.6))
            .when(address.default, |d| d.border_l_2().border_color(p.accent))
            .child(
                div()
                    .w(px(120.))
                    .text_sm()
                    .child(address_label(&address.kind)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(format!("{}/", address.url)),
            )
            .child(if address.kind == "cloudflare" {
                // The public address takes no pairing link.
                div().into_any_element()
            } else if address.default {
                div()
                    .px_2()
                    .py_0p5()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(p.accent)
                    .text_xs()
                    .text_color(p.accent)
                    .child("Default")
                    .into_any_element()
            } else {
                Button::new(SharedString::from(format!("web-address-{host}")))
                    .small()
                    .outline()
                    .label("Set as default")
                    .tooltip("The pairing link uses this address")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_web("link_host", SettingValue::Str(host.clone()), cx);
                    }))
                    .into_any_element()
            })
            .into_any_element()
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

/// Open a link whose fragment is a secret (the pairing token, the
/// website's link key) in the browser. On Linux the URL would be the
/// command line of `xdg-open` and of the browser, which every account on
/// the computer can read (`/proc/<pid>/cmdline`): there the browser is
/// sent to a page of the user's own that goes on to the link instead.
fn open_secret_link(cx: &mut gpui_kit::App, url: &str) {
    #[cfg(target_os = "linux")]
    if let Some(page) = redirect_page(url) {
        cx.open_url(&page);
        return;
    }
    cx.open_url(url);
}

/// Write the page that sends a browser on to `url` (0600, in a folder of
/// the user's own), and return its `file://` address.
#[cfg(target_os = "linux")]
fn redirect_page(url: &str) -> Option<String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = loft_config::private_temp_dir("agentloft-open").ok()?;
    let path = dir.join("link.html");
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(redirect_html(url).as_bytes()).ok()?;
    Some(format!("file://{}", path.display()))
}

/// A page that goes straight on to `url`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn redirect_html(url: &str) -> String {
    let attr: String = url
        .chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '"' => "&quot;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            c => c.to_string(),
        })
        .collect();
    format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\">\
         <meta http-equiv=\"refresh\" content=\"0;url={attr}\"><title>Agentloft</title>\
         <a href=\"{attr}\">Open Agentloft</a>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_link_goes_through_a_page_not_a_command_line() {
        let page = redirect_html("http://127.0.0.1:4780/#token=abc\"><script>x</script>&y");
        assert!(page.contains("url=http://127.0.0.1:4780/#token=abc&quot;&gt;&lt;script&gt;"));
        assert!(!page.contains("<script>"));
        #[cfg(target_os = "linux")]
        {
            let opened = redirect_page("http://127.0.0.1:4780/#token=s3cret").unwrap();
            assert!(opened.starts_with("file://") && !opened.contains("s3cret"));
            let file = std::path::PathBuf::from(opened.trim_start_matches("file://"));
            assert!(std::fs::read_to_string(&file).unwrap().contains("s3cret"));
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn the_qr_code_is_an_svg_of_the_link() {
        let svg = String::from_utf8(qr_svg("http://192.168.1.2:4780/#token=abc").unwrap()).unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("<path fill=\"#000\" d=\"M"));
        assert!(!svg.contains("abc"), "the link itself isn't in the picture");
    }

    #[test]
    fn the_link_is_masked_until_revealed() {
        let link = "http://127.0.0.1:4780/#token=abcDEF123";
        let shown = masked(link);
        assert!(!shown.contains("abcDEF123"));
        assert!(shown.starts_with("http://127.0.0.1:4780/#token="));
        assert_eq!(masked("http://127.0.0.1:4780"), "http://127.0.0.1:4780");
    }
}

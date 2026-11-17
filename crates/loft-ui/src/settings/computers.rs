//! Settings → Other computers: Agentloft on another computer over SSH.
//! Connect to one (typed, connected to before, or from `~/.ssh/config`),
//! forget one, and see whether other computers can connect to this one.
//!
//! Desktop only: a window on another computer is the desktop app's.

use super::{SettingsView, row};
use crate::theme::Palette;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, IntoElement, ParentElement as _,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};

#[derive(Default)]
pub(super) struct ComputersPage {
    /// The host field, and its subscription.
    input: Option<(Entity<InputState>, Subscription)>,
    /// What's wrong with what was typed.
    error: Option<String>,
    /// Whether something answers on this computer's SSH port (Remote Login
    /// on); `None` until looked.
    remote_login: Option<bool>,
    /// What others type to reach this computer (`me@studio.local`).
    reach_me: Option<String>,
    asked: bool,
}

/// Whether SSH answers here, and what others would type to reach this
/// computer. Off the main thread: it waits on a port and runs `scutil`.
fn look_at_this_computer() -> (bool, Option<String>) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 22));
    let listening =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(400)).is_ok();
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty());
    let name = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|n| !n.is_empty())
        .map(|n| format!("{n}.local"));
    let reach = match (user, name) {
        (Some(user), Some(name)) => Some(format!("{user}@{name}")),
        (None, Some(name)) => Some(name),
        _ => None,
    };
    (listening, reach)
}

impl SettingsView {
    fn connect_to(&mut self, host: String, cx: &mut Context<Self>) {
        let host = host.trim().to_owned();
        if !loft_client::valid_destination(&host) {
            self.computers.error = Some(if host.is_empty() {
                "Type a host first.".into()
            } else {
                format!("“{host}” isn't an SSH host: use an alias, host or user@host.")
            });
            cx.notify();
            return;
        }
        self.computers.error = None;
        cx.notify();
        cx.defer(move |cx| {
            crate::windows::open_remote(host, cx);
        });
    }

    pub(super) fn computers_page(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.computers.asked {
            self.computers.asked = true;
            cx.spawn(async move |this, cx| {
                let (listening, reach) = cx
                    .background_executor()
                    .spawn(async { look_at_this_computer() })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.computers.remote_login = Some(listening);
                    this.computers.reach_me = reach;
                    cx.notify();
                });
            })
            .detach();
        }
        if self.computers.input.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("user@host or an alias"));
            let sub = cx.subscribe_in(&input, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let host = state.read(cx).value().to_string();
                    this.connect_to(host, cx);
                }
            });
            self.computers.input = Some((input, sub));
        }
        let input = self
            .computers
            .input
            .as_ref()
            .map(|(i, _)| i.clone())
            .expect("made above");

        let mut page = v_flex().child(super::web::heading(
            "Connect to another computer",
            "Opens a window on the Agentloft of another computer, over SSH. It signs in with your \
             SSH keys and ~/.ssh/config, and opens nothing to the network on either computer.",
            p,
        ));
        let typed = input.clone();
        page = page.child(
            h_flex()
                .pt_3()
                .gap_2()
                .child(div().flex_1().child(Input::new(&input).small()))
                .child(
                    Button::new("computers-connect")
                        .small()
                        .primary()
                        .label("Connect")
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let host = typed.read(cx).value().to_string();
                            this.connect_to(host, cx);
                        })),
                ),
        );
        if let Some(error) = &self.computers.error {
            page = page.child(
                div()
                    .pt_2()
                    .text_xs()
                    .text_color(p.danger)
                    .child(error.clone()),
            );
        }

        // Connected to before, then the rest of ~/.ssh/config.
        let recent = crate::remote::recent(cx);
        let configured: Vec<String> = crate::remote::ssh_config_hosts()
            .into_iter()
            .filter(|h| !recent.contains(h))
            .collect();
        if !recent.is_empty() {
            page = page.child(super::web::heading("Connected before", "", p));
            for host in &recent {
                page = page.child(self.computer_row(host, true, p, cx));
            }
        }
        if !configured.is_empty() {
            page = page.child(super::web::heading("From ~/.ssh/config", "", p));
            for host in &configured {
                page = page.child(self.computer_row(host, false, p, cx));
            }
        }

        // This computer, for the other side.
        let login = match self.computers.remote_login {
            None => "Looking…".to_owned(),
            Some(true) => "On: other computers can sign in to this one over SSH.".to_owned(),
            Some(false) => "Off: turn on Remote Login in System Settings ▸ General ▸ Sharing to \
                            let other computers connect."
                .to_owned(),
        };
        page = page
            .child(super::web::heading(
                "Connecting to this computer",
                "Another computer needs SSH access to this one, and Agentloft installed here \
                 (this version or later). Keep Agentloft open here so agents can use your \
                 sign-ins.",
                p,
            ))
            .child(row(
                "Remote Login",
                &login,
                Button::new("computers-sharing")
                    .small()
                    .outline()
                    .label("Open Sharing settings")
                    .cursor_pointer()
                    .on_click(|_, _, cx| {
                        cx.open_url(
                            "x-apple.systempreferences:com.apple.Sharing-Settings.extension",
                        )
                    })
                    .into_any_element(),
                p,
            ));
        if let Some(reach) = self.computers.reach_me.clone() {
            let copy = reach.clone();
            page = page.child(row(
                "Address",
                &format!("On the other computer, connect to {reach}"),
                Button::new("computers-copy-address")
                    .small()
                    .ghost()
                    .label("Copy")
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                    })
                    .into_any_element(),
                p,
            ));
        }
        page.into_any_element()
    }

    fn computer_row(
        &self,
        host: &str,
        recent: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = crate::windows::is_open_on(host, cx);
        let (connect, forget) = (host.to_owned(), host.to_owned());
        h_flex()
            .py_2p5()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(p.border.opacity(0.6))
            .child(
                gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Server)
                    .small()
                    .text_color(if open { p.success } else { p.muted_foreground }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(host.to_owned()))
                    .when(open, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.muted_foreground)
                                .child("A window is open on it"),
                        )
                    }),
            )
            .when(recent, |d| {
                d.child(
                    Button::new(SharedString::from(format!("computers-forget-{host}")))
                        .small()
                        .ghost()
                        .label("Forget")
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            crate::remote::forget(&forget, cx);
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new(SharedString::from(format!("computers-open-{host}")))
                    .small()
                    .outline()
                    .label(if open { "Show" } else { "Connect" })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.connect_to(connect.clone(), cx);
                    })),
            )
            .w_full()
            .min_h(px(44.))
            .into_any_element()
    }
}

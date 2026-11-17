//! Interactive demo: a `TerminalView` connected to your login shell through
//! `portable-pty`.
//!
//! ```sh
//! cargo run -p loft-terminal --example terminal_demo
//! ```
//!
//! Environment:
//! - `LOFT_TERMINAL_DEMO_THEME=dark|light` picks the theme (default: system).
//! - `LOFT_TERMINAL_DEMO_SCRIPT=<file>` replays a script of UI actions for
//!   automated checks. Every action goes through GPUI's real input pipeline
//!   (key bindings, key listeners, the platform text-input handler, mouse
//!   events). One command per line:
//!
//!   ```text
//!   wait <ms>                  pause
//!   type <text>                type characters as individual keystrokes
//!   key <keystroke>            one keystroke, e.g. `enter`, `ctrl-c`, `cmd-v`
//!   paste <text>               paste text
//!   click <col> <row>          left-click a cell
//!   drag <c1> <r1> <c2> <r2>   left-drag from cell to cell (selection)
//!   wheel <rows>               scroll wheel by lines (positive = up)
//!   resize <width> <height>    resize the window (logical pixels)
//!   theme dark|light           switch theme
//!   mark <name>                print `MARK <name>` (for screenshot harnesses)
//!   dump                       print the visible rows
//!   clipboard                  print the clipboard text
//!   quit                       quit
//!   ```

use futures::StreamExt as _;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, Focusable as _, IntoElement, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, OperatingSystem, ParentElement as _,
    PlatformInput, Point, Render, ScrollDelta, ScrollEvent, Styled as _, Subscription,
    TitlebarOptions, TouchPhase, Window, WindowBounds, WindowOptions, div, point, px, size,
};
use loft_terminal::{TerminalEvent, TerminalView};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read as _, Write as _};
use std::time::Duration;

struct Demo {
    terminal: Entity<TerminalView>,
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _subscriptions: Vec<Subscription>,
}

impl Demo {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> anyhow::Result<Self> {
        let terminal = cx.new(|cx| TerminalView::new(window, cx));

        let pty = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new_default_prog();
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "agentloft");
        if let Some(home) = std::env::var_os("HOME") {
            command.cwd(home);
        }
        let child = pty.slave.spawn_command(command)?;
        drop(pty.slave);

        // PTY output: a blocking reader thread feeds a channel the UI drains.
        let mut reader = pty.master.try_clone_reader()?;
        let (output_tx, mut output_rx) = futures::channel::mpsc::unbounded::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if output_tx.unbounded_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let view = terminal.clone();
        cx.spawn(async move |_, cx| {
            while let Some(chunk) = output_rx.next().await {
                view.update(cx, |terminal, cx| terminal.feed(&chunk, cx));
            }
            // The shell exited.
            cx.update(|cx| cx.quit());
        })
        .detach();

        // PTY input: writes happen off the UI thread.
        let mut writer = pty.master.take_writer()?;
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for bytes in input_rx {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });

        let subscription = cx.subscribe_in(
            &terminal,
            window,
            move |this: &mut Self, _, event, window, _| match event {
                TerminalEvent::Input(bytes) => {
                    let _ = input_tx.send(bytes.clone());
                }
                TerminalEvent::Resize {
                    cols,
                    rows,
                    pixel_width,
                    pixel_height,
                } => {
                    let _ = this.master.resize(PtySize {
                        rows: *rows,
                        cols: *cols,
                        pixel_width: *pixel_width,
                        pixel_height: *pixel_height,
                    });
                }
                TerminalEvent::TitleChanged(title) => {
                    if std::env::var_os("LOFT_TERMINAL_DEMO_SCRIPT").is_some() {
                        println!("TITLE {title}");
                    }
                    window.set_window_title(title);
                }
                TerminalEvent::OpenUrl(url) => println!("OPEN {url}"),
                TerminalEvent::AddToMessage(text) => println!("ADD {text}"),
                TerminalEvent::Bell => {}
            },
        );

        window.focus(&terminal.focus_handle(cx), cx);
        Ok(Self {
            terminal,
            master: pty.master,
            _child: child,
            _subscriptions: vec![subscription],
        })
    }
}

impl Render for Demo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.terminal.clone())
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::centered(None, size(px(900.), px(560.)), cx);
            let opened = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("loft-terminal demo".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    match std::env::var("LOFT_TERMINAL_DEMO_THEME").as_deref() {
                        Ok("dark") => Theme::change(ThemeMode::Dark, Some(window), cx),
                        Ok("light") => Theme::change(ThemeMode::Light, Some(window), cx),
                        _ => Theme::sync_system_appearance(Some(window), cx),
                    }
                    cx.new(|cx| Demo::new(window, cx).expect("failed to start the shell"))
                },
            );
            let (window, demo) = opened.expect("failed to open window");
            cx.activate(true);

            if let Ok(path) = std::env::var("LOFT_TERMINAL_DEMO_SCRIPT") {
                let script = std::fs::read_to_string(&path).expect("read demo script");
                let terminal = demo.read(cx).terminal.clone();
                cx.spawn(async move |cx| {
                    for line in script.lines().map(str::trim) {
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
                        if command == "wait" {
                            let ms = rest.parse().unwrap_or(100);
                            cx.background_executor()
                                .timer(Duration::from_millis(ms))
                                .await;
                            continue;
                        }
                        if command == "probe" {
                            // Main-thread responsiveness: how late do 10 ms
                            // timers fire on the UI thread?
                            let samples: usize = rest.parse().unwrap_or(100);
                            let (mut max, mut total) = (0f64, 0f64);
                            for _ in 0..samples {
                                let start = std::time::Instant::now();
                                cx.background_executor()
                                    .timer(Duration::from_millis(10))
                                    .await;
                                let lag = start.elapsed().as_secs_f64() * 1000. - 10.;
                                max = max.max(lag);
                                total += lag;
                            }
                            println!(
                                "PROBE samples={samples} max_lag_ms={max:.1} avg_lag_ms={:.1}",
                                total / samples as f64
                            );
                            continue;
                        }
                        let terminal = terminal.clone();
                        let (command, rest) = (command.to_owned(), rest.to_owned());
                        let _ = window.update(cx, move |_, window, cx| {
                            run_command(&command, &rest, &terminal, window, cx)
                        });
                        // Let the frame and PTY catch up between actions.
                        cx.background_executor()
                            .timer(Duration::from_millis(30))
                            .await;
                    }
                })
                .detach();
            }
        });
}

fn run_command(
    command: &str,
    rest: &str,
    terminal: &Entity<TerminalView>,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    match command {
        "type" => {
            for c in rest.chars() {
                window.dispatch_keystroke(char_keystroke(c), cx);
            }
        }
        "key" => match Keystroke::parse_with_os(rest, OperatingSystem::Mac) {
            Ok(keystroke) => {
                window.dispatch_keystroke(keystroke, cx);
            }
            Err(err) => eprintln!("bad keystroke {rest:?}: {err}"),
        },
        "paste" => terminal.update(cx, |t, cx| t.paste(rest, cx)),
        "drag" => {
            let n: Vec<u16> = rest
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            let [c1, r1, c2, r2] = n[..] else {
                eprintln!("drag needs 4 numbers");
                return;
            };
            let (Some(from), Some(to)) = (
                terminal.read(cx).cell_bounds(c1, r1),
                terminal.read(cx).cell_bounds(c2, r2),
            ) else {
                return;
            };
            let start = from.origin + point(px(1.), from.size.height / 2.);
            let end = to.origin + point(to.size.width - px(1.), to.size.height / 2.);
            mouse(window, cx, MouseAction::Down, start);
            let mid = point((start.x + end.x) / 2., (start.y + end.y) / 2.);
            mouse(window, cx, MouseAction::Move, mid);
            mouse(window, cx, MouseAction::Move, end);
            mouse(window, cx, MouseAction::Up, end);
        }
        "click" => {
            let n: Vec<u16> = rest
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            let [col, row] = n[..] else {
                eprintln!("click needs 2 numbers");
                return;
            };
            if let Some(cell) = terminal.read(cx).cell_bounds(col, row) {
                let at = cell.center();
                mouse(window, cx, MouseAction::Down, at);
                mouse(window, cx, MouseAction::Up, at);
            }
        }
        "wheel" => {
            let lines: f32 = rest.parse().unwrap_or(1.);
            let center = window.bounds().size;
            window.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: point(center.width / 2., center.height / 2.),
                    delta: ScrollDelta::Lines(point(0., lines)),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                    ..Default::default()
                }),
                cx,
            );
        }
        "resize" => {
            let n: Vec<f32> = rest
                .split_whitespace()
                .filter_map(|v| v.parse().ok())
                .collect();
            if let [w, h] = n[..] {
                window.resize(size(px(w), px(h)));
            }
        }
        "theme" => {
            let mode = if rest == "light" {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            };
            Theme::change(mode, Some(window), cx);
            window.refresh();
        }
        "mark" => println!("MARK {rest}"),
        "dump" => {
            let t = terminal.read(cx);
            let (cols, rows) = t.grid_size();
            println!("DUMP {cols}x{rows} title={:?}", t.title());
            for row in 0..usize::from(rows) {
                println!("| {}", t.row_text(row));
            }
            println!("END DUMP");
        }
        "clipboard" => println!(
            "CLIPBOARD {:?}",
            cx.read_from_clipboard().and_then(|item| item.text())
        ),
        "quit" => cx.quit(),
        other => eprintln!("unknown demo command {other:?}"),
    }
}

/// The keystroke GPUI's macOS backend produces for typing `c`.
fn char_keystroke(c: char) -> Keystroke {
    let mut modifiers = Modifiers::default();
    let key = match c {
        ' ' => "space".to_owned(),
        c if c.is_ascii_uppercase() => {
            modifiers.shift = true;
            c.to_ascii_lowercase().to_string()
        }
        c => c.to_string(),
    };
    Keystroke {
        modifiers,
        key,
        key_char: None,
    }
}

enum MouseAction {
    Down,
    Move,
    Up,
}

fn mouse(
    window: &mut Window,
    cx: &mut gpui_kit::App,
    action: MouseAction,
    at: Point<gpui_kit::Pixels>,
) {
    let event = match action {
        MouseAction::Down => PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
        MouseAction::Move => PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
        MouseAction::Up => PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    };
    window.dispatch_event(event, cx);
}

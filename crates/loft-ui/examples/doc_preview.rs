//! A document's preview in a window of its own, without the app or a daemon:
//! for looking at how a file is drawn, and for trying notes on it.
//!
//! ```sh
//! cargo run -p loft-ui --example doc_preview -- deck.pptx [--width 900] [--height 700]
//!     [--scroll 1200] [--annotate] [--click 300,240]… [--drag 100,200,400,200]…
//!     [--note "Make this blue"] [--hover 300,240] [--find "text"] [--shot out.jpg]
//! ```
//!
//! * `--scroll` starts that many pixels down the document.
//! * `--annotate` turns note-taking on once the document is read.
//! * `--click x,y` clicks at window coordinates and `--drag x1,y1,x2,y2`
//!   drags from one point to another (over an area, or to select text), in
//!   the order given; each is followed by `--note`, if there is one, as the
//!   note's text. Without `--note` the note box stays open.
//! * `--hover x,y` then moves the pointer there with no button held.
//! * `--find` looks for text (in a PDF), as its find bar does.
//! * `--shot` saves the window as drawn (a JPEG) and quits. It needs no
//!   screen-recording permission, but the example must be built with
//!   `--features shots`.
//!
//! Notes are printed as they are made.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, IntoElement, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, PlatformInput, Render, Styled as _,
    Subscription, Window, WindowBounds, WindowOptions, div, point, px, size,
};
use loft_ui::panel::docs::{DocEvent, DocPreview, doc_kind};
use std::time::Duration;

struct Demo {
    preview: Entity<DocPreview>,
    _notes: Subscription,
}

impl Render for Demo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors.background)
            .text_color(cx.theme().colors.foreground)
            .child(self.preview.clone())
    }
}

#[derive(Default)]
struct Script {
    scroll: f32,
    annotate: bool,
    /// Where the pointer goes down and where it comes up (the same for a click).
    clicks: Vec<((f32, f32), (f32, f32))>,
    note: Option<String>,
    hover: Option<(f32, f32)>,
    find: Option<String>,
    shot: Option<String>,
}

#[cfg(feature = "shots")]
fn save(window: &Window, path: &str) -> anyhow::Result<()> {
    let frame = window.render_to_image()?;
    let rgb = image::DynamicImage::ImageRgba8(frame).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&rgb)?;
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(not(feature = "shots"))]
fn save(_window: &Window, _path: &str) -> anyhow::Result<()> {
    anyhow::bail!("--shot needs the example built with `--features shots`")
}

fn main() {
    let mut path = None;
    let (mut width, mut height) = (900f32, 700f32);
    let mut script = Script::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut number = || args.next().and_then(|v| v.parse::<f32>().ok());
        match arg.as_str() {
            "--width" => width = number().unwrap_or(width),
            "--height" => height = number().unwrap_or(height),
            "--scroll" => script.scroll = number().unwrap_or(0.),
            "--annotate" => script.annotate = true,
            "--click" | "--drag" => {
                let at = args.next().unwrap_or_default();
                let n: Vec<f32> = at
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect();
                match n.as_slice() {
                    [x, y] => script.clicks.push(((*x, *y), (*x, *y))),
                    [x1, y1, x2, y2] => script.clicks.push(((*x1, *y1), (*x2, *y2))),
                    _ => {}
                }
            }
            "--note" => script.note = args.next(),
            "--hover" => {
                let at = args.next().unwrap_or_default();
                script.hover = at
                    .split_once(',')
                    .and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)));
            }
            "--find" => script.find = args.next(),
            "--shot" => script.shot = args.next(),
            _ => path = Some(arg),
        }
    }
    let path = path.expect("usage: doc_preview <file> [options]; see the top of the example");
    let kind = doc_kind(&path).expect("not a document this preview reads");
    let bytes = std::fs::read(&path).expect("cannot read the file");

    gpui_kit::application()
        .with_assets(loft_ui::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    let preview = cx.new(|cx| DocPreview::new(kind, &path, bytes, window, cx));
                    cx.new(|cx| {
                        let notes = cx.subscribe(&preview, |_, _, event: &DocEvent, _| {
                            let DocEvent::Note(note) = event;
                            println!(
                                "NOTE {} [{}] On {}: {}",
                                note.n, note.chip, note.what, note.note
                            );
                        });
                        run(script, preview.clone(), window, cx);
                        Demo {
                            preview,
                            _notes: notes,
                        }
                    })
                },
            )
            .expect("failed to open window");
            cx.activate(true);
        });
}

/// Plays the options out: wait for the document, scroll, click, note, shoot.
fn run(script: Script, preview: Entity<DocPreview>, window: &mut Window, cx: &mut Context<Demo>) {
    cx.spawn_in(window, async move |_, cx| {
        let executor = cx.background_executor().clone();
        let pause = |ms: u64| executor.timer(Duration::from_millis(ms));
        // With the display asleep nothing asks the window for frames: draw
        // them here, so clicks land on what is there and the picture saved
        // is the current one.
        let draw = |cx: &mut gpui_kit::AsyncWindowContext| {
            let _ = cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        // The document is read off the main thread.
        for _ in 0..200 {
            pause(50).await;
            if preview.read_with(cx, |p, _| p.can_annotate()) {
                break;
            }
        }
        preview.update(cx, |p, cx| {
            p.scroll_to(px(script.scroll));
            if script.annotate {
                p.set_annotating(true, cx);
            }
            cx.notify();
        });
        pause(700).await;
        draw(cx);
        for (from, to) in script.clicks {
            draw(cx);
            let (down, up) = (point(px(from.0), px(from.1)), point(px(to.0), px(to.1)));
            let (button, modifiers) = (MouseButton::Left, Modifiers::default());
            let mut steps = vec![PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: down,
                modifiers,
                click_count: 1,
                first_mouse: false,
            })];
            if from != to {
                // A drag passes through the points between.
                for i in 1..=8 {
                    let t = i as f32 / 8.;
                    steps.push(PlatformInput::MouseMove(MouseMoveEvent {
                        position: point(
                            px(from.0 + (to.0 - from.0) * t),
                            px(from.1 + (to.1 - from.1) * t),
                        ),
                        pressed_button: Some(button),
                        modifiers,
                    }));
                }
            }
            steps.push(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: up,
                modifiers,
                click_count: 1,
            }));
            for step in steps {
                let _ = cx.update(|window, cx| {
                    window.dispatch_event(step, cx);
                });
                pause(60).await;
            }
            pause(250).await;
            if let Some(note) = &script.note {
                let _ = cx.update(|window, cx| {
                    preview.update(cx, |p, cx| p.finish_note(note, window, cx));
                });
                pause(250).await;
            }
        }
        if let Some((x, y)) = script.hover {
            let _ = cx.update(|window, cx| {
                let moved = PlatformInput::MouseMove(MouseMoveEvent {
                    position: point(px(x), px(y)),
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                });
                window.dispatch_event(moved, cx);
            });
            pause(250).await;
        }
        if let Some(query) = &script.find {
            let _ = cx.update(|window, cx| {
                preview.update(cx, |p, cx| p.find(query, window, cx));
            });
            // Every page's text is read first, then the view moves.
            pause(1200).await;
        }
        if let Some(shot) = script.shot {
            // Pictures (slide images, PDF pages) arrive a little after the
            // first frame.
            for _ in 0..4 {
                draw(cx);
                pause(400).await;
            }
            draw(cx);
            let saved = cx.update(|window, _| save(window, &shot));
            match saved {
                Ok(Ok(())) => println!("SHOT {shot}"),
                Ok(Err(err)) => eprintln!("no picture: {err:#}"),
                Err(err) => eprintln!("no picture: {err:#}"),
            }
            let _ = cx.update(|_, cx| cx.quit());
        }
    })
    .detach();
}

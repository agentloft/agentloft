//! Headless GPUI tests for `TerminalView`: real key dispatch (bindings, key
//! listeners and the text input handler), layout-driven resize, clipboard,
//! IME and mouse reporting, on GPUI's test platform.
//!
//! Needs this crate's `test-support` feature (see Cargo.toml for why it is
//! not on by default): `cargo test -p loft-terminal --features test-support`.

use gpui_kit::{
    ClipboardItem, Entity, EntityInputHandler as _, Focusable as _, Modifiers, MouseButton,
    TestAppContext, VisualTestContext, px, size,
};
use loft_terminal::{TerminalEvent, TerminalView};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

type Events = Rc<RefCell<Vec<TerminalEvent>>>;

fn setup(cx: &mut TestAppContext) -> (Entity<TerminalView>, &mut VisualTestContext, Events) {
    cx.update(gpui_kit::init);
    let events: Events = Rc::default();
    let sink = events.clone();
    let (view, cx) = cx.add_window_view(move |window, cx| {
        let view = TerminalView::new(window, cx);
        cx.subscribe_self(move |_, event: &TerminalEvent, _| sink.borrow_mut().push(event.clone()))
            .detach();
        view
    });
    cx.update(|window, cx| {
        window.activate_window();
        window.focus(&view.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (view, cx, events)
}

/// Feed output and let the flush task and a frame run.
fn feed(view: &Entity<TerminalView>, cx: &mut VisualTestContext, bytes: &[u8]) {
    view.update(cx, |t, cx| t.feed(bytes, cx));
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn inputs(events: &Events) -> Vec<Vec<u8>> {
    events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            TerminalEvent::Input(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

#[gpui_kit::test]
fn first_layout_emits_one_resize(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    let resizes = || {
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, TerminalEvent::Resize { .. }))
            .cloned()
            .collect::<Vec<_>>()
    };
    let (cols, rows) = view.read_with(cx, |t, _| t.grid_size());
    assert!(cols > 20 && rows > 5, "grid {cols}x{rows}");
    assert_eq!(resizes().len(), 1, "{:?}", events.borrow());
    match &resizes()[0] {
        TerminalEvent::Resize {
            cols: c, rows: r, ..
        } => assert_eq!((*c, *r), (cols, rows)),
        _ => unreachable!(),
    }
    // Re-rendering at the same size does not emit again.
    feed(&view, cx, b"x");
    assert_eq!(resizes().len(), 1);
    // A smaller window shrinks the grid and emits once.
    cx.simulate_resize(size(px(400.), px(200.)));
    cx.run_until_parked();
    let (small_cols, small_rows) = view.read_with(cx, |t, _| t.grid_size());
    assert!(small_cols < cols && small_rows < rows);
    assert_eq!(resizes().len(), 2);
    match resizes().last().unwrap() {
        TerminalEvent::Resize {
            cols,
            rows,
            pixel_width,
            pixel_height,
        } => {
            assert_eq!((*cols, *rows), (small_cols, small_rows));
            assert!(*pixel_width > 0 && *pixel_height > 0);
        }
        _ => unreachable!(),
    }
}

#[gpui_kit::test]
fn feed_updates_rows_title_and_bell(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    feed(
        &view,
        cx,
        b"hello\r\n\x1b[31mworld\x1b[0m\x1b]2;my title\x07\x07",
    );
    view.read_with(cx, |t, _| {
        assert_eq!(t.row_text(0), "hello");
        assert_eq!(t.row_text(1), "world");
        assert_eq!(t.title(), Some("my title"));
        assert_eq!(t.cursor_position(), Some((5, 1)));
    });
    let events = events.borrow();
    assert!(events.contains(&TerminalEvent::TitleChanged("my title".into())));
    assert!(events.contains(&TerminalEvent::Bell));
}

#[gpui_kit::test]
fn query_replies_are_sent_as_input(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    feed(&view, cx, b"\x1b[3;7H\x1b[6n");
    assert!(inputs(&events).contains(&b"\x1b[3;7R".to_vec()));
}

#[gpui_kit::test]
fn reset_clears_screen_and_title(cx: &mut TestAppContext) {
    let (view, cx, _) = setup(cx);
    feed(&view, cx, b"\x1b]2;t\x07some text");
    view.update(cx, |t, cx| t.reset(cx));
    feed(&view, cx, b"replayed");
    view.read_with(cx, |t, _| {
        assert_eq!(t.row_text(0), "replayed");
        assert_eq!(t.title(), None);
    });
}

#[gpui_kit::test]
fn keystrokes_are_encoded_through_gpui_dispatch(cx: &mut TestAppContext) {
    let (_view, cx, events) = setup(cx);
    // Text keys go through the platform text path (input handler),
    // named keys and chords through the key listener.
    cx.simulate_keystrokes("a shift-b enter tab shift-tab backspace escape up ctrl-c ctrl-d f5");
    assert_eq!(
        inputs(&events),
        vec![
            b"a".to_vec(),
            b"B".to_vec(),
            b"\r".to_vec(),
            b"\t".to_vec(),
            b"\x1b[Z".to_vec(),
            b"\x7f".to_vec(),
            b"\x1b".to_vec(),
            b"\x1b[A".to_vec(),
            b"\x03".to_vec(),
            b"\x04".to_vec(),
            b"\x1b[15~".to_vec(),
        ]
    );
}

#[gpui_kit::test]
fn application_cursor_mode_changes_arrow_encoding(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    feed(&view, cx, b"\x1b[?1h");
    events.borrow_mut().clear();
    cx.simulate_keystrokes("up left");
    assert_eq!(
        inputs(&events),
        vec![b"\x1bOA".to_vec(), b"\x1bOD".to_vec()]
    );
}

#[gpui_kit::test]
fn paste_uses_bracketed_mode_when_enabled(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    cx.write_to_clipboard(ClipboardItem::new_string("echo hi\nls".into()));
    cx.simulate_keystrokes("cmd-v");
    feed(&view, cx, b"\x1b[?2004h");
    cx.simulate_keystrokes("cmd-v");
    assert_eq!(
        inputs(&events),
        vec![
            b"echo hi\rls".to_vec(),
            b"\x1b[200~echo hi\nls\x1b[201~".to_vec()
        ]
    );
}

#[gpui_kit::test]
fn select_all_and_copy(cx: &mut TestAppContext) {
    let (view, cx, _) = setup(cx);
    feed(&view, cx, b"first line\r\nsecond line");
    cx.simulate_keystrokes("cmd-a cmd-c");
    let copied = cx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(copied.as_deref(), Some("first line\nsecond line"));
    // Typing clears the selection.
    cx.simulate_keystrokes("x");
    assert_eq!(view.read_with(cx, |t, _| t.selection_text()), None);
}

#[gpui_kit::test]
fn mouse_drag_selects_text(cx: &mut TestAppContext) {
    let (view, cx, _) = setup(cx);
    feed(&view, cx, b"alpha beta gamma");
    let (start, end) = view.read_with(cx, |t, _| {
        (t.cell_bounds(6, 0).unwrap(), t.cell_bounds(9, 0).unwrap())
    });
    let from = start.origin + gpui_kit::point(px(1.), start.size.height / 2.);
    let to = end.origin + gpui_kit::point(end.size.width - px(1.), end.size.height / 2.);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    assert_eq!(
        view.read_with(cx, |t, _| t.selection_text()).as_deref(),
        Some("beta")
    );
}

#[gpui_kit::test]
fn mouse_reporting_when_the_program_asks(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    feed(&view, cx, b"\x1b[?1000h\x1b[?1006h");
    events.borrow_mut().clear();
    let cell = view.read_with(cx, |t, _| t.cell_bounds(2, 3).unwrap());
    cx.simulate_click(cell.center(), Modifiers::default());
    assert_eq!(
        inputs(&events),
        vec![b"\x1b[<0;3;4M".to_vec(), b"\x1b[<0;3;4m".to_vec()]
    );
    // Shift overrides reporting so text can still be selected.
    events.borrow_mut().clear();
    cx.simulate_click(
        cell.center(),
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    );
    assert!(inputs(&events).is_empty());
}

#[gpui_kit::test]
fn ime_composition_then_commit(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    cx.update(|window, cx| {
        view.update(cx, |t, cx| {
            t.replace_and_mark_text_in_range(None, "にほ", None, window, cx);
            assert_eq!(t.marked_text_range(window, cx), Some(0..2));
        })
    });
    assert!(inputs(&events).is_empty(), "composition is not sent");
    cx.update(|window, cx| {
        view.update(cx, |t, cx| {
            t.replace_text_in_range(None, "日本", window, cx);
            assert_eq!(t.marked_text_range(window, cx), None);
        })
    });
    assert_eq!(inputs(&events), vec!["日本".as_bytes().to_vec()]);
}

#[gpui_kit::test]
fn focus_reports_when_enabled(cx: &mut TestAppContext) {
    let (view, cx, events) = setup(cx);
    feed(&view, cx, b"\x1b[?1004h");
    events.borrow_mut().clear();
    cx.update(|window, cx| window.blur(cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.focus(&view.focus_handle(cx), cx));
    cx.run_until_parked();
    assert_eq!(
        inputs(&events),
        vec![b"\x1b[O".to_vec(), b"\x1b[I".to_vec()]
    );
}

#[gpui_kit::test]
fn large_feed_is_processed_in_slices(cx: &mut TestAppContext) {
    let (view, cx, _) = setup(cx);
    let mut bytes = Vec::new();
    for i in 0..200_000 {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    for chunk in bytes.chunks(65_536) {
        view.update(cx, |t, cx| t.feed(chunk, cx));
    }
    // Parsing yields between time-boxed slices on a short timer; drive the
    // simulated clock until the queue drains.
    let rows = view.read_with(cx, |t, _| t.grid_size().1);
    let mut last = String::new();
    for _ in 0..1_000 {
        cx.executor().advance_clock(Duration::from_millis(2));
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        last = view.read_with(cx, |t, _| t.row_text(usize::from(rows) - 2));
        if last == "line 199999" {
            break;
        }
    }
    assert_eq!(last, "line 199999");
}

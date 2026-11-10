//! Headless tests against the real libghostty-vt engine: feed escape
//! sequences, read the render frame back, and check encoders.

use super::ghostty::GhosttyBackend;
use super::*;
use std::time::Duration;

const DEFAULT_FG: Rgb = Rgb::hex(0xd0d0d0);
const DEFAULT_BG: Rgb = Rgb::hex(0x101010);

fn ansi() -> [Rgb; 16] {
    let mut ansi = [Rgb::default(); 16];
    for (i, c) in ansi.iter_mut().enumerate() {
        // Distinct, recognizable values: 0x0i0i0i style.
        let v = (i as u8) * 16 + 1;
        *c = Rgb::new(v, v.wrapping_add(1), v.wrapping_add(2));
    }
    ansi
}

fn backend(cols: u16, rows: u16) -> GhosttyBackend {
    let mut b = GhosttyBackend::new(cols, rows).expect("create terminal");
    b.set_colors(&Colors {
        foreground: DEFAULT_FG,
        background: DEFAULT_BG,
        cursor: Rgb::hex(0xffffff),
        ansi: ansi(),
    });
    b
}

fn frame_of(b: &mut GhosttyBackend) -> Frame {
    let mut frame = Frame::default();
    b.update_frame(&mut frame, true);
    frame
}

fn cell(frame: &Frame, col: usize, row: usize) -> &Cell {
    &frame.lines[row].cells[col]
}

#[test]
fn plain_text_lands_in_the_grid() {
    let mut b = backend(20, 5);
    b.write(b"hello\r\nworld");
    let frame = frame_of(&mut b);
    assert_eq!((frame.cols, frame.rows), (20, 5));
    assert_eq!(frame.row_text(0), "hello");
    assert_eq!(frame.row_text(1), "world");
    assert_eq!(frame.row_text(2), "");
    assert_eq!(frame.cursor.position, Some((5, 1)));
    assert_eq!(frame.foreground, DEFAULT_FG);
    assert_eq!(frame.background, DEFAULT_BG);
}

#[test]
fn cursor_addressing_and_erase() {
    let mut b = backend(10, 3);
    b.write(b"abcdefghij\x1b[1;4HXY\x1b[2;1Hrow2\x1b[1;8H\x1b[K");
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(0), "abcXYfg");
    assert_eq!(frame.row_text(1), "row2");
}

#[test]
fn sixteen_colors_resolve_through_the_palette() {
    let mut b = backend(20, 2);
    b.write(b"\x1b[31mR\x1b[0m\x1b[92mG\x1b[0m\x1b[44mB\x1b[0mN");
    let frame = frame_of(&mut b);
    let pal = ansi();
    assert_eq!(cell(&frame, 0, 0).fg, Some(pal[1]));
    assert_eq!(cell(&frame, 1, 0).fg, Some(pal[10]));
    assert_eq!(cell(&frame, 2, 0).bg, Some(pal[4]));
    let plain = cell(&frame, 3, 0);
    assert_eq!((plain.fg, plain.bg), (None, None));
}

#[test]
fn indexed_256_and_truecolor() {
    let mut b = backend(20, 2);
    b.write(b"\x1b[38;5;196ma\x1b[48;5;21mb\x1b[0m\x1b[38;2;1;2;3;48;2;250;128;7mc");
    let frame = frame_of(&mut b);
    // xterm cube: 196 = (5,0,0) -> ff0000, 21 = (0,0,5) -> 0000ff.
    assert_eq!(cell(&frame, 0, 0).fg, Some(Rgb::hex(0xff0000)));
    assert_eq!(cell(&frame, 1, 0).bg, Some(Rgb::hex(0x0000ff)));
    assert_eq!(cell(&frame, 2, 0).fg, Some(Rgb::new(1, 2, 3)));
    assert_eq!(cell(&frame, 2, 0).bg, Some(Rgb::new(250, 128, 7)));
}

#[test]
fn sgr_attributes() {
    let mut b = backend(20, 2);
    b.write(
        b"\x1b[1mb\x1b[0m\x1b[3mi\x1b[0m\x1b[4mu\x1b[0m\x1b[9ms\x1b[0m\
          \x1b[7mv\x1b[0m\x1b[2md\x1b[0m\x1b[4:3mc\x1b[0m\x1b[21mw\x1b[0m\
          \x1b[8mh\x1b[0m\x1b[53mo\x1b[0m\x1b[4;58;2;9;8;7mx\x1b[0m",
    );
    let frame = frame_of(&mut b);
    let attrs = |col| cell(&frame, col, 0).attrs;
    assert!(attrs(0).bold);
    assert!(attrs(1).italic);
    assert_eq!(attrs(2).underline, Underline::Single);
    assert!(attrs(3).strikethrough);
    assert!(attrs(4).inverse);
    assert!(attrs(5).faint);
    assert_eq!(attrs(6).underline, Underline::Curly);
    assert_eq!(attrs(7).underline, Underline::Double);
    assert!(attrs(8).invisible);
    assert!(attrs(9).overline);
    assert_eq!(cell(&frame, 10, 0).underline_color, Some(Rgb::new(9, 8, 7)));
    // Reset really resets.
    b.write(b"\x1b[1;3;4mX\x1b[mY");
    let frame = frame_of(&mut b);
    assert_eq!(cell(&frame, 12, 0).attrs, Attrs::default());
}

#[test]
fn wide_characters_take_two_cells() {
    let mut b = backend(10, 2);
    b.write("中a😀b".as_bytes());
    let frame = frame_of(&mut b);
    assert_eq!(cell(&frame, 0, 0).ch, '中');
    assert_eq!(cell(&frame, 0, 0).width, CellWidth::Wide);
    assert_eq!(cell(&frame, 1, 0).width, CellWidth::SpacerTail);
    assert_eq!(cell(&frame, 2, 0).ch, 'a');
    assert_eq!(cell(&frame, 3, 0).ch, '😀');
    assert_eq!(cell(&frame, 3, 0).width, CellWidth::Wide);
    assert_eq!(cell(&frame, 5, 0).ch, 'b');
    assert_eq!(frame.row_text(0), "中a😀b");
    assert_eq!(frame.cursor.position, Some((6, 0)));
}

#[test]
fn wide_character_wraps_with_spacer_head() {
    let mut b = backend(5, 3);
    b.write("abcd中".as_bytes());
    let frame = frame_of(&mut b);
    assert_eq!(cell(&frame, 4, 0).width, CellWidth::SpacerHead);
    assert_eq!(cell(&frame, 0, 1).ch, '中');
}

#[test]
fn combining_marks_stay_in_one_cell() {
    let mut b = backend(10, 2);
    b.write("e\u{301}x".as_bytes());
    let frame = frame_of(&mut b);
    assert_eq!(cell(&frame, 0, 0).ch, 'e');
    assert_eq!(cell(&frame, 0, 0).extra.as_deref(), Some("\u{301}"));
    assert_eq!(cell(&frame, 1, 0).ch, 'x');
}

#[test]
fn alternate_screen_switches_and_restores() {
    let mut b = backend(20, 4);
    b.write(b"primary\r\n$ ");
    assert!(!b.modes().alt_screen);
    b.write(b"\x1b[?1049h\x1b[H\x1b[2Jfullscreen app");
    assert!(b.modes().alt_screen);
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(0), "fullscreen app");
    assert_eq!(frame.row_text(1), "");
    b.write(b"\x1b[?1049l");
    assert!(!b.modes().alt_screen);
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(0), "primary");
    assert_eq!(frame.row_text(1), "$");
    assert_eq!(frame.cursor.position, Some((2, 1)));
}

#[test]
fn damage_reports_only_changed_rows() {
    let mut b = backend(20, 6);
    let mut frame = Frame::default();
    b.write(b"one\r\ntwo\r\nthree");
    assert_eq!(b.update_frame(&mut frame, false), Damage::Full);
    assert_eq!(b.update_frame(&mut frame, false), Damage::None);
    b.write(b"\x1b[2;1Hxyz");
    // Row 1 changed; row 2 is damaged too because the cursor left it.
    match b.update_frame(&mut frame, false) {
        Damage::Rows(rows) => {
            assert!(rows.contains(&1), "{rows:?}");
            assert!(!rows.contains(&0), "{rows:?}");
        }
        other => panic!("expected row damage, got {other:?}"),
    }
    assert_eq!(frame.row_text(1), "xyz");
    assert_eq!(frame.row_text(2), "three");
}

#[test]
fn scrollback_and_viewport_scrolling() {
    let mut b = backend(10, 3);
    for i in 0..10 {
        b.write(format!("line{i}\r\n").as_bytes());
    }
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(0), "line8");
    assert_eq!(frame.row_text(1), "line9");
    assert!(b.is_at_bottom());
    b.scroll(Scroll::Delta(-3));
    assert!(!b.is_at_bottom());
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(0), "line5");
    assert_eq!(frame.cursor.position, None, "cursor scrolled out of view");
    let bar = b.scrollbar();
    assert_eq!(bar.len, 3);
    assert!(bar.total >= 11, "{bar:?}");
    b.scroll(Scroll::Top);
    assert_eq!(frame_of(&mut b).row_text(0), "line0");
    b.scroll(Scroll::Bottom);
    assert!(b.is_at_bottom());
    assert_eq!(frame_of(&mut b).row_text(0), "line8");
}

#[test]
fn resize_reflows_the_primary_screen() {
    let mut b = backend(10, 3);
    b.write(b"abcdefghij");
    b.resize(5, 3, 8, 16);
    assert_eq!(b.size(), (5, 3));
    let frame = frame_of(&mut b);
    assert_eq!((frame.cols, frame.rows), (5, 3));
    assert_eq!(frame.row_text(0), "abcde");
    assert_eq!(frame.row_text(1), "fghij");
}

#[test]
fn effects_title_bell_and_query_replies() {
    let mut b = backend(20, 3);
    b.write(b"\x1b]2;my title\x07\x07\x1b[2;3H\x1b[6n");
    assert_eq!(b.title(), Some("my title"));
    let effects = b.take_effects();
    assert!(effects.contains(&Effect::TitleChanged("my title".into())));
    assert!(effects.contains(&Effect::Bell));
    assert!(
        effects.contains(&Effect::PtyWrite(b"\x1b[2;3R".to_vec())),
        "{effects:?}"
    );
    assert!(b.take_effects().is_empty());
}

#[test]
fn reset_clears_everything() {
    let mut b = backend(10, 3);
    b.write(b"\x1b]0;t\x07text\x1b[?2004h\x1b[?1049h");
    b.reset();
    assert_eq!(b.title(), None);
    let modes = b.modes();
    assert!(!modes.bracketed_paste && !modes.alt_screen && !modes.mouse_tracking);
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(0), "");
}

#[test]
fn cursor_shape_and_visibility() {
    let mut b = backend(10, 3);
    b.write(b"\x1b[6 q");
    let f = frame_of(&mut b);
    assert_eq!(f.cursor.shape, CursorShape::Bar);
    assert!(f.cursor.visible);
    b.write(b"\x1b[4 q\x1b[?25l");
    let f = frame_of(&mut b);
    assert_eq!(f.cursor.shape, CursorShape::Underline);
    assert!(!f.cursor.visible);
}

#[test]
fn selection_gesture_and_copy_text() {
    let mut b = backend(20, 3);
    b.write(b"hello world\r\nsecond line");
    let geometry = Geometry {
        cols: 20,
        rows: 3,
        cell_width: 10,
        cell_height: 20,
    };
    // Like native text selection, a cell joins the selection once the
    // pointer crosses its midpoint: press on the left half, drag to the
    // right half.
    let at = |col: u16, row: u16| GridPoint {
        col,
        row,
        x: f32::from(col) * 10. + 2.,
        y: f32::from(row) * 20. + 5.,
    };
    let to = |col: u16, row: u16| GridPoint {
        x: f32::from(col) * 10. + 8.,
        ..at(col, row)
    };
    b.selection_press(at(0, 0), Duration::from_secs(10));
    b.selection_drag(to(4, 0), geometry, false);
    b.selection_release(Some(to(4, 0)));
    assert!(b.has_selection());
    assert_eq!(b.selection_text().as_deref(), Some("hello"));
    let frame = frame_of(&mut b);
    assert_eq!(frame.lines[0].selection, Some((0, 4)));
    assert_eq!(frame.lines[1].selection, None);

    // Double click selects a word.
    b.selection_press(at(7, 0), Duration::from_secs(20));
    b.selection_release(Some(at(7, 0)));
    b.selection_press(at(7, 0), Duration::from_millis(20_100));
    b.selection_release(Some(at(7, 0)));
    assert_eq!(b.selection_text().as_deref(), Some("world"));

    // Multi-line drag, trailing blanks trimmed.
    b.selection_press(at(6, 0), Duration::from_secs(30));
    b.selection_drag(to(5, 1), geometry, false);
    assert_eq!(b.selection_text().as_deref(), Some("world\nsecond"));

    b.clear_selection();
    assert!(!b.has_selection());
    b.select_all();
    assert_eq!(
        b.selection_text().as_deref(),
        Some("hello world\nsecond line")
    );
}

fn key(name: &str) -> KeyInput {
    KeyInput {
        key: name.into(),
        ..KeyInput::default()
    }
}

fn text_key(name: &str, text: &str) -> KeyInput {
    KeyInput {
        key: name.into(),
        text: Some(text.into()),
        ..KeyInput::default()
    }
}

fn with_mods(mut input: KeyInput, f: impl FnOnce(&mut Mods)) -> KeyInput {
    f(&mut input.mods);
    input
}

#[test]
fn key_encoding_legacy() {
    let mut b = backend(20, 3);
    assert_eq!(b.encode_key(&text_key("a", "a")), b"a");
    let shifted = KeyInput {
        consumed: Mods {
            shift: true,
            ..Mods::default()
        },
        ..with_mods(text_key("a", "A"), |m| m.shift = true)
    };
    assert_eq!(b.encode_key(&shifted), b"A");
    assert_eq!(b.encode_key(&key("enter")), b"\r");
    assert_eq!(b.encode_key(&key("tab")), b"\t");
    assert_eq!(b.encode_key(&key("backspace")), b"\x7f");
    assert_eq!(b.encode_key(&key("escape")), b"\x1b");
    assert_eq!(b.encode_key(&key("up")), b"\x1b[A");
    assert_eq!(b.encode_key(&key("left")), b"\x1b[D");
    assert_eq!(b.encode_key(&key("home")), b"\x1b[H");
    assert_eq!(b.encode_key(&key("pageup")), b"\x1b[5~");
    assert_eq!(b.encode_key(&key("delete")), b"\x1b[3~");
    assert_eq!(b.encode_key(&key("f1")), b"\x1bOP");
    assert_eq!(b.encode_key(&key("f5")), b"\x1b[15~");
    assert_eq!(b.encode_key(&key("f12")), b"\x1b[24~");
    assert_eq!(
        b.encode_key(&with_mods(key("tab"), |m| m.shift = true)),
        b"\x1b[Z"
    );
    assert_eq!(
        b.encode_key(&with_mods(key("left"), |m| m.ctrl = true)),
        b"\x1b[1;5D"
    );
    assert_eq!(
        b.encode_key(&with_mods(key("up"), |m| m.shift = true)),
        b"\x1b[1;2A"
    );
    // Ctrl combinations produce C0 controls.
    assert_eq!(
        b.encode_key(&with_mods(text_key("c", "c"), |m| m.ctrl = true)),
        b"\x03"
    );
    assert_eq!(
        b.encode_key(&with_mods(text_key("d", "d"), |m| m.ctrl = true)),
        b"\x04"
    );
    assert_eq!(
        b.encode_key(&with_mods(text_key("2", "2"), |m| m.ctrl = true)),
        b"\x00"
    );
    // Ghostty follows fixterms: ctrl+[ / ctrl+i / ctrl+m are CSI u so
    // programs can tell them apart from Esc / Tab / Enter.
    assert_eq!(
        b.encode_key(&with_mods(text_key("[", "["), |m| m.ctrl = true)),
        b"\x1b[91;5u"
    );
    // Release events produce nothing in legacy mode.
    let release = KeyInput {
        action: KeyAction::Release,
        ..key("enter")
    };
    assert!(b.encode_key(&release).is_empty());
    // Bare modifiers produce nothing.
    assert!(b.encode_key(&key("shift")).is_empty());
}

#[test]
fn key_encoding_alt_as_meta() {
    let mut b = backend(20, 3);
    b.set_option_as_alt(true);
    assert_eq!(
        b.encode_key(&with_mods(text_key("b", "b"), |m| m.alt = true)),
        b"\x1bb"
    );
    assert_eq!(
        b.encode_key(&with_mods(key("backspace"), |m| m.alt = true)),
        b"\x1b\x7f"
    );
    assert_eq!(
        b.encode_key(&with_mods(key("right"), |m| m.alt = true)),
        b"\x1b[1;3C"
    );
}

#[test]
fn key_encoding_follows_terminal_modes() {
    let mut b = backend(20, 3);
    // DECCKM: application cursor keys.
    b.write(b"\x1b[?1h");
    assert_eq!(b.encode_key(&key("up")), b"\x1bOA");
    b.write(b"\x1b[?1l");
    assert_eq!(b.encode_key(&key("up")), b"\x1b[A");
    // Kitty keyboard protocol (disambiguate): escape becomes CSI u.
    b.write(b"\x1b[>1u");
    assert_eq!(b.encode_key(&key("escape")), b"\x1b[27u");
    assert_eq!(
        b.encode_key(&with_mods(text_key("c", "c"), |m| m.ctrl = true)),
        b"\x1b[99;5u"
    );
    assert_eq!(b.encode_key(&text_key("a", "a")), b"a");
    b.write(b"\x1b[<u");
    assert_eq!(b.encode_key(&key("escape")), b"\x1b");
}

#[test]
fn paste_encoding() {
    let mut b = backend(20, 3);
    assert_eq!(b.encode_paste("ls\nwc"), b"ls\rwc");
    b.write(b"\x1b[?2004h");
    assert!(b.modes().bracketed_paste);
    assert_eq!(b.encode_paste("ls\nwc"), b"\x1b[200~ls\nwc\x1b[201~");
    // Control bytes that could escape the bracket are neutralized.
    let encoded = b.encode_paste("a\x1b[201~b");
    assert!(encoded.starts_with(b"\x1b[200~"));
    assert!(encoded.ends_with(b"\x1b[201~"));
    assert_eq!(encoded.iter().filter(|&&c| c == 0x1b).count(), 2);
    let big = "x".repeat(100_000);
    assert_eq!(b.encode_paste(&big).len(), 100_000 + 12);
}

#[test]
fn mouse_reporting() {
    let mut b = backend(20, 5);
    let geometry = Geometry {
        cols: 20,
        rows: 5,
        cell_width: 10,
        cell_height: 20,
    };
    let press = MouseInput {
        action: MouseAction::Press,
        button: Some(MouseButton::Left),
        mods: Mods::default(),
        x: 25.,
        y: 65.,
        any_button_pressed: true,
    };
    assert!(!b.modes().mouse_tracking);
    assert!(b.encode_mouse(&press, geometry).is_empty());

    b.write(b"\x1b[?1000h\x1b[?1006h");
    assert!(b.modes().mouse_tracking);
    assert_eq!(b.encode_mouse(&press, geometry), b"\x1b[<0;3;4M");
    let release = MouseInput {
        action: MouseAction::Release,
        any_button_pressed: false,
        ..press
    };
    assert_eq!(b.encode_mouse(&release, geometry), b"\x1b[<0;3;4m");
    let wheel = MouseInput {
        button: Some(MouseButton::WheelUp),
        ..press
    };
    assert_eq!(b.encode_mouse(&wheel, geometry), b"\x1b[<64;3;4M");
    // Normal tracking (1000) does not report motion.
    let motion = MouseInput {
        action: MouseAction::Motion,
        button: None,
        any_button_pressed: false,
        x: 55.,
        ..press
    };
    assert!(b.encode_mouse(&motion, geometry).is_empty());
    // Any-event tracking (1003) does, once per cell.
    b.write(b"\x1b[?1003h");
    assert_eq!(b.encode_mouse(&motion, geometry), b"\x1b[<35;6;4M");
    let same_cell = MouseInput { x: 57., ..motion };
    assert!(b.encode_mouse(&same_cell, geometry).is_empty());
}

#[test]
fn focus_reporting() {
    let mut b = backend(20, 3);
    assert!(b.encode_focus(true).is_empty());
    b.write(b"\x1b[?1004h");
    assert_eq!(b.encode_focus(true), b"\x1b[I");
    assert_eq!(b.encode_focus(false), b"\x1b[O");
}

#[test]
fn synchronized_output_holds_rendering() {
    let mut b = backend(20, 3);
    let mut frame = Frame::default();
    b.write(b"before");
    b.update_frame(&mut frame, false);
    b.write(b"\x1b[?2026h\r\x1b[Kafter");
    assert_eq!(b.update_frame(&mut frame, false), Damage::None);
    assert_eq!(frame.row_text(0), "before");
    b.write(b"\x1b[?2026l");
    assert_ne!(b.update_frame(&mut frame, false), Damage::None);
    assert_eq!(frame.row_text(0), "after");
}

#[test]
fn large_output_streams_without_losing_state() {
    let mut b = backend(80, 24);
    let mut chunk = Vec::new();
    for i in 0..20_000 {
        chunk.extend_from_slice(format!("\x1b[3{}mline {i:05}\x1b[0m\r\n", i % 8).as_bytes());
    }
    for piece in chunk.chunks(4096) {
        b.write(piece);
    }
    let frame = frame_of(&mut b);
    assert_eq!(frame.row_text(22), "line 19999");
    assert_eq!(frame.row_text(23), "");
}

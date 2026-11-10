//! Generate `src/bindings.rs` from the pinned Ghostty headers.
//!
//! Run through `scripts/sync.sh`, or directly with
//! `cargo run -p ghostty-vt-sys --features bindgen-tool --bin gen-bindings`.
//! The headers come from `GHOSTTY_INCLUDE_DIR`, `GHOSTTY_SOURCE_DIR/include`,
//! or the `third_party/ghostty` submodule, in that order.
//!
//! Adapted from libghostty-vt-sys 0.2.2 (MIT OR Apache-2.0).

use std::env;
use std::path::PathBuf;

use bindgen::EnumVariation;
use bindgen::callbacks::{EnumVariantValue, IntKind, ItemInfo, ItemKind, ParseCallbacks};

use heck::ToShoutySnakeCase;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"));

    let include_dir = if let Ok(dir) = env::var("GHOSTTY_INCLUDE_DIR") {
        PathBuf::from(dir)
    } else if let Ok(src) = env::var("GHOSTTY_SOURCE_DIR") {
        PathBuf::from(src).join("include")
    } else {
        let submodule = manifest_dir
            .join("..")
            .join("..")
            .join("third_party")
            .join("ghostty")
            .join("include");
        assert!(
            submodule.join("ghostty").join("vt.h").exists(),
            "could not find ghostty headers; check out the third_party/ghostty submodule, \
             or set GHOSTTY_INCLUDE_DIR or GHOSTTY_SOURCE_DIR"
        );
        submodule
    };

    let header = include_dir.join("ghostty").join("vt.h");
    // Agentloft: `GHOSTTY_BINDINGS_TARGET=wasm32-unknown-unknown` writes the
    // browser build's bindings (32-bit layouts) next to the native ones.
    let target = env::var("GHOSTTY_BINDINGS_TARGET").ok();
    let out = manifest_dir.join("src").join(match target.as_deref() {
        Some(target) if target.starts_with("wasm32") => "bindings_wasm32.rs",
        Some(other) => panic!("no bindings file for target `{other}`"),
        None => "bindings.rs",
    });

    let mut builder = bindgen::Builder::default();
    if let Some(target) = &target {
        // Clang hides symbols by default on wasm32-unknown-unknown, and
        // bindgen leaves hidden functions out.
        builder = builder
            .clang_arg(format!("--target={target}"))
            .clang_arg("-ffreestanding")
            .clang_arg("-fvisibility=default");
    }
    let builder = builder
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", include_dir.to_string_lossy()))
        // The crate links the static archive, so the headers must not
        // carry dllimport or visibility attributes.
        .clang_arg("-DGHOSTTY_STATIC")
        .allowlist_function("[Gg]hostty.*")
        .allowlist_type("[Gg]hostty.*")
        .allowlist_var("GHOSTTY_.*")
        .generate_cstr(true)
        .derive_default(true)
        .size_t_is_usize(true)
        .default_enum_style(EnumVariation::ModuleConsts)
        .parse_callbacks(Box::new(Callbacks));

    let bindings = builder
        .generate()
        .expect("failed to generate bindings from include/ghostty/vt.h");

    bindings
        .write_to_file(&out)
        .unwrap_or_else(|error| panic!("failed to write bindings to {}: {error}", out.display()));
}

const PREFIXES: &[(&str, &str)] = &[
    ("GhosttyOptimizeMode", "GHOSTTY_OPTIMIZE"),
    ("GhosttyKeyEncoderOption", "GHOSTTY_KEY_ENCODER_OPT"),
    ("GhosttyMouseTrackingMode", "GHOSTTY_MOUSE_TRACKING"),
    ("GhosttyMouseEncoderOption", "GHOSTTY_MOUSE_ENCODER_OPT"),
    ("GhosttySgrAttributeTag", "GHOSTTY_SGR_ATTR"),
    ("GhosttyOscCommandData", "GHOSTTY_OSC_DATA"),
    ("GhosttyOscCommandType", "GHOSTTY_OSC_COMMAND"),
    ("GhosttyTerminalOption", "GHOSTTY_TERMINAL_OPT"),
    (
        "GhosttyTerminalScrollViewportTag",
        "GHOSTTY_SCROLL_VIEWPORT",
    ),
    ("GhosttyStyleColorTag", "GHOSTTY_STYLE_COLOR"),
    ("GhosttyRowSemanticPrompt", "GHOSTTY_ROW_SEMANTIC"),
    ("GhosttyCellSemanticContent", "GHOSTTY_CELL_SEMANTIC"),
    ("GhosttyCellContentTag", "GHOSTTY_CELL_CONTENT"),
    ("GhosttySizeReportStyle", "GHOSTTY_SIZE_REPORT"),
    ("GhosttyModeReportState", "GHOSTTY_MODE_REPORT"),
    ("GhosttyFocusEvent", "GHOSTTY_FOCUS"),
    ("GhosttyResult", "GHOSTTY_"),
    ("GhosttyKittyGraphicsImageData", "GHOSTTY_KITTY_IMAGE_DATA"),
    (
        "GhosttySelectionGestureEventOption",
        "GHOSTTY_SELECTION_GESTURE_EVENT_OPT",
    ),
    ("GhosttySearchOption", "GHOSTTY_SEARCH_OPT"),
    (
        "GhosttySnapshotDecoderOption",
        "GHOSTTY_SNAPSHOT_DECODER_OPT",
    ),
    ("GhosttySysOption", "GHOSTTY_SYS_OPT"),
    (
        "GhosttyTerminalUnknownSequenceTag",
        "GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE",
    ),
];

#[derive(Debug)]
struct Callbacks;

impl ParseCallbacks for Callbacks {
    fn item_name(&self, item_info: ItemInfo) -> Option<String> {
        let prefix = match item_info.kind {
            // Do not rename functions since bindgen unconditionally prefixes
            // the `link_name` with `\u{1}`, which breaks on Apple platforms.
            // See https://github.com/rust-lang/rust-bindgen/issues/1221
            ItemKind::Function => return None,
            ItemKind::Var => "GHOSTTY_",
            _ => "Ghostty",
        };
        Some(item_info.name.trim_start_matches(prefix).to_string())
    }

    fn enum_variant_name(
        &self,
        enum_name: Option<&str>,
        original_variant_name: &str,
        _variant_value: EnumVariantValue,
    ) -> Option<String> {
        let enum_name = enum_name?;

        // Remove redundant C prefixes
        let prefix = PREFIXES
            .iter()
            .find(|(v, _)| *v == enum_name)
            .map(|(_, n)| n.to_string())
            .unwrap_or(enum_name.to_shouty_snake_case());

        let transformed = original_variant_name
            .trim_start_matches(&prefix)
            .trim_start_matches('_');

        Some(transformed.to_string())
    }

    fn process_comment(&self, comment: &str) -> Option<String> {
        Some(
            comment
                .lines()
                // Ignore doxygen directives.
                .filter(|s| !s.trim().starts_with("@"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn int_macro(&self, name: &str, _value: i64) -> Option<IntKind> {
        // Fixup some int macro types to reduce manual casting
        if name.starts_with("GHOSTTY_DA_") || name.starts_with("GHOSTTY_MODS_") {
            Some(IntKind::U16)
        } else if name.starts_with("GHOSTTY_KITTY_KEY_") || name.starts_with("GHOSTTY_COLOR_NAMED_")
        {
            Some(IntKind::U8)
        } else {
            None
        }
    }
}

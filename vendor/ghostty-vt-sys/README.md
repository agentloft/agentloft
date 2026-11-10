# ghostty-vt-sys (Agentloft's copy)

This is `ghostty-vt-sys` 0.8.0 from crates.io (Apache-2.0, from
[gpui-cn](https://github.com/prabirshrestha/gpui-cn)), with a build for
`wasm32-unknown-unknown` added so that the terminal runs in Agentloft's
browser client (docs/adr/0016-web-client.md). The workspace patches it in
for every target (`[patch.crates-io]` in the root `Cargo.toml`); on native
targets it builds exactly what the published crate builds.

What differs from the published crate:

- `build.rs`: `wasm32-unknown-unknown` maps to Zig's `wasm32-freestanding`,
  built with `-Dsimd=false` (Ghostty's SIMD paths need a libc) and
  `-Doptimize=ReleaseSmall`. A wasm build installs the archive but no
  headers, so the include path is the source tree's.
- `src/bindings_wasm32.rs`: bindings generated for a 32-bit target. bindgen
  checks struct layouts at compile time, and the published bindings carry
  the 64-bit ones. `src/lib.rs` picks the file by target.
- `tools/gen_bindings.rs`: `GHOSTTY_BINDINGS_TARGET` writes that file.
- `Cargo.toml`: stands alone (the original inherits from its workspace).

To regenerate the wasm32 bindings (needs libclang; on macOS the command
line tools have it), from the repository root, with `GHOSTTY_SOURCE_DIR`
set to an extracted Ghostty source tarball for the commit in
`GHOSTTY.lock` (a native build leaves one under
`target/debug/build/ghostty-vt-sys-*/out/src`):

```sh
LIBCLANG_PATH=/Library/Developer/CommandLineTools/usr/lib \
GHOSTTY_INCLUDE_DIR="$GHOSTTY_SOURCE_DIR/include" \
GHOSTTY_BINDINGS_TARGET=wasm32-unknown-unknown \
cargo run --manifest-path vendor/ghostty-vt-sys/Cargo.toml \
  --features bindgen-tool --bin gen-bindings
```

When `ghostty-vt` moves to a new version: copy the matching published
`ghostty-vt-sys` here, reapply the changes above (they are marked
"Agentloft" in the sources), and regenerate. When the published crate
builds for wasm32 itself, delete this folder and the patch.

The original README follows.

---

# ghostty-vt-sys

Raw FFI bindings for [libghostty-vt](https://ghostty.org), the terminal
emulation library extracted from the Ghostty terminal emulator.

This crate is currently developed primarily for
[`gpui-cn`](https://github.com/prabirshrestha/gpui-cn).

The crate version normally follows gpui-cn and records its Ghostty source as
build metadata. For example, `0.5.0+ghostty.1.3.2-main.b40acce` uses Ghostty
`1.3.2-main` at commit `b40acce`.

The build script builds libghostty-vt from Ghostty's official
`libghostty-vt-source.tar.gz` for the pinned commit with Zig 0.17 and links
the static archive. `src/bindings.rs` is generated from the pinned headers
and committed, so the crate needs no bindgen or Clang to build.

Most users want the safe crate, [`ghostty-vt`](https://crates.io/crates/ghostty-vt).

## Requirements

- Zig 0.17.x on `PATH` or in `ZIG`. In this repository, the build also finds
  the version from `mise.toml` through `mise`.
- On macOS, the Xcode command line tools.
- Network on the first build, or `GHOSTTY_SOURCE_DIR` and
  `GHOSTTY_ZIG_SYSTEM_DIR` for offline builds.

## Environment variables

| Variable | Effect |
| --- | --- |
| `GHOSTTY_SOURCE_DIR` | Build from this Ghostty source tree. |
| `GHOSTTY_SOURCE_URL` | Download the tarball from this mirror. It must match the pinned sha256 in `GHOSTTY.lock`. |
| `GHOSTTY_ZIG_SYSTEM_DIR` | Resolve Zig packages from this directory (`zig build --system`) instead of fetching. |
| `GHOSTTY_VT_CPU` | Zig CPU model, default `baseline`. |
| `ZIG` | Path to the Zig binary. |
| `DOCS_RS` | Skip the native build. |

## Features

- `pkg-config`: use an installed `libghostty-vt-static` found through
  pkg-config before downloading. libghostty-vt is pre-1.0, so an installed
  library must match the pinned commit's C API.
- `bindgen-tool`: builds the `gen-bindings` binary that regenerates
  `src/bindings.rs`.

## Targets

x86_64 and aarch64 for Linux (glibc, musl), macOS and Windows (MSVC, built
natively on Windows), plus `aarch64-apple-ios` and `aarch64-apple-ios-sim`.
macOS is the only supported runtime in this release; the others build in CI.

## License

Apache-2.0. Ghostty is MIT (see `LICENSE-GHOSTTY`). Adapted from
libghostty-vt-sys 0.2.2 (MIT OR Apache-2.0).

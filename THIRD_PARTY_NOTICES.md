# Third-party notices

Agentloft is licensed under Apache-2.0. Parts of it are adapted from, or link
against, third-party software under the licenses below (ADR-0009). Crates
pulled in through Cargo carry their own license metadata, which `cargo deny`
checks; this file covers code adapted into this repository and native code
that is statically linked into Agentloft binaries.

## Built-in colour themes

The built-in themes in `crates/loft-ui/src/themes.rs` are our own, with
palettes modelled on these MIT-licensed editor themes (no code is used):
GitHub Primer (Studio), Catppuccin (Lavender), Nord (Glacier), Rosé Pine
(Dusk), Gruvbox (Harvest), Everforest (Evergreen), Tokyo Night (Neon),
Atom One (Slate) and Dracula (Nightshade). Thanks to their authors.

## Fonts in the web client

The browser has no access to system fonts, so the web client
(`crates/loft-web/fonts/`) ships its own, each under the SIL Open Font
License 1.1 (license texts are alongside the files):

- Inter 4.1, https://github.com/rsms/inter (`Inter-LICENSE.txt`): the
  interface font.
- JetBrains Mono 2.304, https://github.com/JetBrains/JetBrainsMono
  (`JetBrainsMono-OFL.txt`): code, diffs and tool output.
- IBM Plex Sans, https://github.com/IBM/plex (`IBMPlexSans-LICENSE.txt`):
  the family GPUI's web platform falls back to when a font is missing.

The font files are unmodified.

## Simple Icons

- Project: Simple Icons, https://simpleicons.org (`simple-icons` 15.0.0)
- License: CC0-1.0
- Used in: `crates/loft-ui/assets/brand/` (`claude.svg`, `openai.svg`,
  `gemini.svg`), shown to identify each agent. The marks are trademarks of
  their owners (Anthropic, OpenAI, Google). `opencode.svg` is our own glyph.

## Google Antigravity

- Source: [Google Antigravity Press Assets](https://antigravity.google/press)
  (Icon - Full Color).
- Used in: `crates/loft-ui/assets/brand/antigravity.png`. The transparent
  margins were cropped and the image resized for the agent picker. The mark
  is a trademark of Google.

## Material Icon Theme

- Project: Material Icon Theme, https://github.com/material-extensions/vscode-material-icon-theme (`material-icon-theme` 5.40.0)
- License: MIT
- Used in: `crates/loft-ui/assets/file-icons/` (a curated subset of its SVG
  icons) and the generated `crates/loft-ui/src/file_icons_table.rs`
  (file name, extension and folder mappings). Regenerate with
  `python3 xtask/scripts/file_icons.py`.

```
The MIT License (MIT)
Copyright (c) 2025 Material Extensions

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

## Ghostty (libghostty-vt)

- Project: Ghostty, https://github.com/ghostty-org/ghostty
- License: MIT
- Used in: `crates/loft-terminal`, through the `ghostty-vt` / `ghostty-vt-sys`
  crates (Apache-2.0), which build libghostty-vt from Ghostty's source
  tarball (commit `83fb95624b0439ee660c2679b43b38c8b63769cd`, 1.4.0) and link
  it statically, into the desktop app and into the web client's wasm. No
  Ghostty source is copied into this repository.
- `vendor/ghostty-vt-sys` is a copy of `ghostty-vt-sys` 0.8.0 (Apache-2.0,
  from https://github.com/prabirshrestha/gpui-cn) with a wasm32 build added;
  its README lists the changes. Its bindings files are generated from
  Ghostty's C headers.

```
Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

//! Labelled screenshots: the interactive elements in view are found by a
//! script in the helpers' isolated world (nothing is added to the page),
//! then numbered on the picture itself, in tags drawn over its pixels.

use image::{Rgba, RgbaImage};
use serde_json::Value;

/// Labels when the agent doesn't say how many.
const DEFAULT_LABELS: u32 = 80;
/// The most labels a picture gets.
const MOST_LABELS: u32 = 300;

/// How many labels: what was asked, within 1..=300.
pub(super) fn how_many(asked: Option<u32>) -> u32 {
    asked.unwrap_or(DEFAULT_LABELS).clamp(1, MOST_LABELS)
}

/// The script (an async function body for the helpers' world) that lists
/// up to `max` interactive elements in view, as JSON.
pub(super) fn script(max: u32) -> String {
    format!("const MAX = {max};\n{FIND_JS}")
}

const FIND_JS: &str = r#"
const ROLES = new Set(['button', 'link', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio',
  'option', 'switch', 'checkbox', 'radio', 'combobox', 'searchbox', 'textbox', 'slider',
  'spinbutton', 'treeitem']);
const CONTAINERS = 'tr,[role=row],li,[role=listitem],fieldset,section,article,form,[aria-label],[aria-labelledby]';
const squash = (s) => String(s == null ? '' : s).replace(/\s+/g, ' ').trim();
const cut = (s, n) => Array.from(s).slice(0, n).join('');

function interactive(el) {
  const tag = el.localName;
  if (tag === 'button' || tag === 'input' || tag === 'textarea' || tag === 'select' || tag === 'summary') return true;
  if (tag === 'a' && el.hasAttribute('href')) return true;
  const role = (el.getAttribute('role') || '').trim().toLowerCase();
  if (ROLES.has(role)) return true;
  return el.hasAttribute('onclick') || el.hasAttribute('contenteditable');
}

function roleOf(el) {
  const role = (el.getAttribute('role') || '').trim();
  if (role) return role;
  const tag = el.localName;
  if (tag === 'a') return 'link';
  if (tag === 'button') return 'button';
  if (tag === 'input') return el.getAttribute('type') || 'input';
  if (tag === 'textarea' || tag === 'select') return tag;
  if (el.hasAttribute('contenteditable') || el.isContentEditable) return 'contenteditable';
  if (el.hasAttribute('onclick')) return 'onclick';
  return tag;
}

function textOf(el) {
  const tag = el.localName;
  const value = (tag === 'input' && (el.type || '').toLowerCase() !== 'password') ||
    tag === 'textarea' || tag === 'select' ? el.value : '';
  for (const candidate of [el.getAttribute('aria-label'), el.getAttribute('placeholder'), value,
    el.getAttribute('title'), el.textContent]) {
    const text = squash(candidate);
    if (text) return cut(text, 80);
  }
  return '';
}

function parentOf(el) {
  if (el.parentElement) return el.parentElement;
  const root = el.getRootNode();
  return root && root.host ? root.host : null;
}

function containerText(box) {
  const own = squash(box.getAttribute('aria-label'));
  if (own) return own;
  const ids = squash(box.getAttribute('aria-labelledby'));
  if (ids) {
    const root = box.getRootNode();
    const named = ids.split(' ').map((id) => {
      const target = root.getElementById ? root.getElementById(id) : document.getElementById(id);
      return target ? squash(target.textContent) : '';
    }).filter(Boolean).join(' ');
    if (named) return named;
  }
  for (const child of box.children) {
    if (box.localName === 'fieldset' && child.localName === 'legend') {
      const legend = squash(child.textContent);
      if (legend) return legend;
    }
  }
  for (const child of box.children) {
    if (/^h[1-6]$/.test(child.localName)) {
      const heading = squash(child.textContent);
      if (heading) return heading;
    }
  }
  return squash(box.textContent);
}

function contextOf(el, rect) {
  for (let up = parentOf(el); up; up = parentOf(up)) {
    if (up.matches && up.matches(CONTAINERS)) return containerText(up);
  }
  // No container: the nearest heading that ends above the element.
  let best = null;
  let bestBottom = -Infinity;
  for (const heading of document.querySelectorAll('h1,h2,h3,h4,h5,h6,[role=heading]')) {
    const r = heading.getBoundingClientRect();
    if (r.bottom > rect.top || r.right <= 0 || r.left >= innerWidth) continue;
    if (r.bottom > bestBottom) {
      best = heading;
      bestBottom = r.bottom;
    }
  }
  return best ? squash(best.textContent) : '';
}

function cssString(value) {
  return '"' + String(value).replace(/["\\]/g, '\\$&').replace(/\n/g, '\\a ') + '"';
}

function nthOfType(el, siblings) {
  let n = 0;
  for (const other of siblings) {
    if (other.localName === el.localName) n++;
    if (other === el) return n;
  }
  return n;
}

// A selector for `el` within its own document or shadow root.
function selectorWithin(el) {
  if (el.id) return '#' + CSS.escape(el.id);
  for (const attr of ['data-testid', 'data-test-id', 'data-test']) {
    if (el.hasAttribute(attr)) return '[' + attr + '=' + cssString(el.getAttribute(attr)) + ']';
  }
  const tag = el.localName;
  if (el.hasAttribute('name')) return tag + '[name=' + cssString(el.getAttribute('name')) + ']';
  const parent = el.parentElement;
  if (parent) return selectorWithin(parent) + ' > ' + tag + ':nth-of-type(' + nthOfType(el, parent.children) + ')';
  const root = el.getRootNode();
  if (root instanceof ShadowRoot) return tag + ':nth-of-type(' + nthOfType(el, root.children) + ')';
  return tag;
}

function selectorOf(el) {
  const root = el.getRootNode();
  const own = selectorWithin(el);
  return root instanceof ShadowRoot && root.host ? selectorOf(root.host) + ' >> ' + own : own;
}

function shown(el) {
  const rect = el.getBoundingClientRect();
  if (rect.width < 4 || rect.height < 4) return null;
  if (rect.right <= 0 || rect.bottom <= 0 || rect.left >= innerWidth || rect.top >= innerHeight) return null;
  const style = getComputedStyle(el);
  if (style.visibility === 'hidden' || style.display === 'none' || Number(style.opacity) === 0) return null;
  return rect;
}

const labels = [];
function visit(el) {
  if (labels.length >= MAX) return;
  if (interactive(el)) {
    const rect = shown(el);
    if (rect) {
      const text = textOf(el);
      const label = {
        label: labels.length + 1,
        role: roleOf(el),
        text,
        selector: selectorOf(el),
        rect: [rect.x, rect.y, rect.width, rect.height].map(Math.round),
      };
      const context = cut(contextOf(el, rect), 60);
      if (context && context.toLowerCase() !== cut(text, 60).toLowerCase()) label.context = context;
      labels.push(label);
    }
  }
  if (el.shadowRoot) for (const child of el.shadowRoot.children) visit(child);
  for (const child of el.children) visit(child);
}
if (document.documentElement) visit(document.documentElement);
return JSON.stringify(labels);
"#;

/// The labels the script found, each with its `rect` as numbers.
pub(super) fn parse(reply: &str) -> Result<Vec<Value>, String> {
    let mut labels: Vec<Value> =
        serde_json::from_str(reply).map_err(|e| format!("unreadable labels: {e}"))?;
    for label in &mut labels {
        // Whole numbers, kept as numbers with a fraction (`300.0`).
        let rect: Vec<f64> = label["rect"]
            .as_array()
            .map(|r| {
                r.iter()
                    .map(|n| n.as_f64().unwrap_or(0.0).round())
                    .collect()
            })
            .unwrap_or_default();
        if rect.len() != 4 {
            return Err("unreadable labels: a label without its box".into());
        }
        label["rect"] = serde_json::json!(rect);
    }
    Ok(labels)
}

const BORDER: Rgba<u8> = Rgba([0x7c, 0x3a, 0xed, 0xff]);
const FILL: Rgba<u8> = Rgba([0xa7, 0x8b, 0xfa, 0xff]);
const INK: Rgba<u8> = Rgba([0x11, 0x18, 0x27, 0xff]);
/// A tag's height, in CSS pixels.
const TAG: f64 = 18.0;

/// Digits on a 5×7 grid of dots, a row a byte (the low 5 bits, the
/// leftmost dot highest).
const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ],
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
    ],
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ],
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ],
];

/// Draw each label's numbered tag on `image`, whose pixels are `scale`
/// times the CSS pixels of the labels' boxes. The elements themselves are
/// left as they are.
pub(super) fn draw(image: &mut RgbaImage, labels: &[Value], scale: f64) {
    let dot = (1.6 * scale).round().max(1.0) as i64;
    let border = scale.round().max(1.0) as i64;
    for label in labels {
        let number = label["label"].as_u64().unwrap_or(0).to_string();
        let rect = &label["rect"];
        let (x, y) = (
            rect[0].as_f64().unwrap_or(0.0),
            rect[1].as_f64().unwrap_or(0.0),
        );
        let digits = number.len() as i64;
        let text_width = (6 * digits - 1) * dot;
        let text_height = 7 * dot;
        let left = (x.max(0.0) * scale).round() as i64;
        let top = ((y - TAG).max(0.0) * scale).round() as i64;
        let width = ((TAG * scale).round() as i64).max(text_width + (8.0 * scale).round() as i64);
        let height = (TAG * scale).round() as i64;
        fill(image, left, top, width, height, BORDER);
        fill(
            image,
            left + border,
            top + border,
            width - 2 * border,
            height - 2 * border,
            FILL,
        );
        let text_left = left + (width - text_width) / 2;
        let text_top = top + (height - text_height) / 2;
        for (n, digit) in number.bytes().enumerate() {
            let glyph = &DIGITS[usize::from(digit - b'0')];
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..5 {
                    if bits & (0b10000 >> column) != 0 {
                        fill(
                            image,
                            text_left + (n as i64 * 6 + column) * dot,
                            text_top + row as i64 * dot,
                            dot,
                            dot,
                            INK,
                        );
                    }
                }
            }
        }
    }
}

/// Fill a rectangle, clipped to the image.
fn fill(image: &mut RgbaImage, x: i64, y: i64, width: i64, height: i64, colour: Rgba<u8>) {
    let (w, h) = (i64::from(image.width()), i64::from(image.height()));
    for py in y.max(0)..(y + height).min(h) {
        for px in x.max(0)..(x + width).min(w) {
            image.put_pixel(px as u32, py as u32, colour);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tag_sits_above_its_element() {
        let mut image = RgbaImage::from_pixel(60, 40, Rgba([255, 255, 255, 255]));
        draw(
            &mut image,
            &[json!({ "label": 1, "rect": [10.0, 20.0, 30.0, 10.0] })],
            1.0,
        );
        assert_eq!(*image.get_pixel(10, 2), BORDER);
        assert_eq!(image.get_pixel(20, 25).0, [255, 255, 255, 255]);
        assert_eq!(image.get_pixel(55, 35).0, [255, 255, 255, 255]);
        // Ink inside the tag; the fill around it.
        let tag: Vec<_> = (2..20)
            .flat_map(|y| (10..28).map(move |x| (x, y)))
            .map(|(x, y)| *image.get_pixel(x, y))
            .collect();
        assert!(tag.contains(&INK) && tag.contains(&FILL));
        // Off the image: clipped, no panic.
        draw(
            &mut image,
            &[json!({ "label": 123, "rect": [55.0, -30.0, 30.0, 10.0] })],
            2.0,
        );
    }

    #[test]
    fn the_script_keeps_to_its_maximum() {
        assert_eq!(how_many(None), 80);
        assert_eq!(how_many(Some(0)), 1);
        assert_eq!(how_many(Some(1000)), 300);
        assert!(script(how_many(Some(1000))).starts_with("const MAX = 300;"));
        assert!(FIND_JS.contains("if (labels.length >= MAX) return;"));
        let labels = parse(r##"[{"label":1,"role":"button","text":"Go","selector":"#go","rect":[300,60,120,40]}]"##)
            .unwrap();
        assert_eq!(labels[0]["rect"], json!([300.0, 60.0, 120.0, 40.0]));
    }
}

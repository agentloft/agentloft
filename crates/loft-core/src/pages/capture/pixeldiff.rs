//! Images and screenshot diffs: reading an image's size from its header,
//! decoding and encoding PNGs within limits, and comparing two pictures
//! pixel by pixel.
//!
//! The comparison counts a pixel as changed as pixelmatch does with
//! anti-aliased pixels included: the perceptual YIQ colour distance of
//! Kotsarenko and Ramos ("Measuring perceived color difference using YIQ
//! NTSC transmission color space in mobile applications", 2010), against a
//! tolerance that grows with the square of the threshold. Changed pixels
//! are then gathered into boxes on a coarse grid, so an agent learns where
//! things changed, not only how much.

use image::{ImageFormat, RgbaImage};

/// The largest PNG decoded, in bytes.
pub(super) const MAX_PNG_BYTES: usize = 32 * 1024 * 1024;
/// The most pixels an image decoded (or stitched) may have.
pub(super) const MAX_PIXELS: u64 = 16 * 1024 * 1024;
/// Changed pixels are gathered on a grid of cells this many pixels a side.
const CELL: u32 = 16;
/// The most changed regions listed.
pub(super) const MAX_REGIONS: usize = 20;
/// The largest squared YIQ distance two colours can have.
const MAX_DELTA: f64 = 35215.0;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Width and height of a PNG (from its IHDR) or a JPEG (from its
/// start-of-frame header); `None` for anything else.
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        // The first chunk is IHDR: length, type, then width and height.
        let header = bytes.get(8..24)?;
        if &header[4..8] != b"IHDR" {
            return None;
        }
        let width = u32::from_be_bytes(header[8..12].try_into().ok()?);
        let height = u32::from_be_bytes(header[12..16].try_into().ok()?);
        return Some((width, height));
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return jpeg_size(bytes);
    }
    None
}

/// A JPEG's size: the segments are walked up to the first frame header.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    loop {
        // Markers may be padded with any number of 0xFF bytes.
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        while *bytes.get(at)? == 0xFF {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        // Markers that stand alone, without a length.
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            // The end, or the image data, before any frame header.
            return None;
        }
        let length = u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize;
        if length < 2 {
            return None;
        }
        // Start of frame: every 0xCn but DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let segment = bytes.get(at + 2..at + 7)?;
            let height = u16::from_be_bytes([segment[1], segment[2]]) as u32;
            let width = u16::from_be_bytes([segment[3], segment[4]]) as u32;
            return Some((width, height));
        }
        at += length;
    }
}

/// Decode a PNG within the limits a diff accepts.
pub(super) fn decode_png(bytes: &[u8]) -> Result<RgbaImage, String> {
    if bytes.len() > MAX_PNG_BYTES || !bytes.starts_with(&PNG_SIGNATURE) {
        return Err("the image must be a PNG no larger than 32 MiB".into());
    }
    let (width, height) = image_size(bytes)
        .ok_or_else(|| "the PNG could not be decoded: it has no readable header".to_owned())?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels == 0 || pixels > MAX_PIXELS {
        return Err(format!(
            "the image ({width}×{height}) is over the 16 megapixel limit: take a smaller region, or scale \"css\""
        ));
    }
    image::load_from_memory_with_format(bytes, ImageFormat::Png)
        .map(|image| image.to_rgba8())
        .map_err(|e| format!("the PNG could not be decoded: {e}"))
}

/// Encode an image as a PNG.
pub(super) fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|e| format!("couldn't encode the PNG: {e}"))?;
    Ok(out.into_inner())
}

/// A box of changed pixels, in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Changed {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: u64,
}

/// What a comparison found.
pub(super) struct Diff {
    pub changed: u64,
    /// The baseline faded nearly to white, changed pixels red.
    pub image: RgbaImage,
    /// The biggest regions of change, top to bottom.
    pub regions: Vec<Changed>,
}

/// Compare `current` with `baseline`, pixel for pixel; images are never
/// resized to compare.
pub(super) fn compare(
    baseline: &RgbaImage,
    current: &RgbaImage,
    threshold: f64,
    max_regions: usize,
) -> Result<Diff, String> {
    let (width, height) = baseline.dimensions();
    if current.dimensions() != (width, height) {
        let (cw, ch) = current.dimensions();
        return Err(format!(
            "screenshot sizes differ: baseline {width}×{height}, current {cw}×{ch}. Compare the \
             same region at the same viewport size and scale; images are never resized."
        ));
    }
    let tolerance = MAX_DELTA * threshold * threshold;
    let columns = width.div_ceil(CELL);
    let rows = height.div_ceil(CELL);
    let mut cells = vec![0u64; (columns * rows) as usize];
    let mut image = RgbaImage::new(width, height);
    let mut changed = 0u64;
    for ((x, y, before), after) in baseline.enumerate_pixels().zip(current.pixels()) {
        let differs = before != after && distance(before.0, after.0) > tolerance;
        let out = if differs {
            changed += 1;
            cells[((y / CELL) * columns + x / CELL) as usize] += 1;
            [255, 0, 0, 255]
        } else {
            let [r, g, b, a] = before.0;
            let alpha = 0.1 * f64::from(a) / 255.0;
            let grey = blend(luma(f64::from(r), f64::from(g), f64::from(b)), alpha)
                .round()
                .clamp(0.0, 255.0) as u8;
            [grey, grey, grey, 255]
        };
        image.put_pixel(x, y, image::Rgba(out));
    }
    let regions = regions(&cells, columns, rows, width, height, max_regions);
    Ok(Diff {
        changed,
        image,
        regions,
    })
}

/// A colour channel blended toward white by `alpha`.
fn blend(channel: f64, alpha: f64) -> f64 {
    255.0 + (channel - 255.0) * alpha
}

fn luma(r: f64, g: f64, b: f64) -> f64 {
    r * 0.298_895_31 + g * 0.586_622_47 + b * 0.114_482_23
}

fn in_phase(r: f64, g: f64, b: f64) -> f64 {
    r * 0.595_977_99 - g * 0.274_176_10 - b * 0.321_801_89
}

fn quadrature(r: f64, g: f64, b: f64) -> f64 {
    r * 0.211_470_17 - g * 0.522_617_11 + b * 0.311_146_94
}

/// The squared perceptual distance of two RGBA colours, each first laid on
/// white by its alpha.
fn distance(one: [u8; 4], other: [u8; 4]) -> f64 {
    let on_white = |[r, g, b, a]: [u8; 4]| {
        let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
        if a == 255 {
            return (r, g, b);
        }
        let alpha = f64::from(a) / 255.0;
        (blend(r, alpha), blend(g, alpha), blend(b, alpha))
    };
    let (r1, g1, b1) = on_white(one);
    let (r2, g2, b2) = on_white(other);
    let y = luma(r1, g1, b1) - luma(r2, g2, b2);
    let i = in_phase(r1, g1, b1) - in_phase(r2, g2, b2);
    let q = quadrature(r1, g1, b1) - quadrature(r2, g2, b2);
    0.5053 * y * y + 0.299 * i * i + 0.1957 * q * q
}

/// Cells with changes that touch (corners too) make one region; the
/// biggest `max` are kept, listed top to bottom.
fn regions(
    cells: &[u64],
    columns: u32,
    rows: u32,
    width: u32,
    height: u32,
    max: usize,
) -> Vec<Changed> {
    let mut seen = vec![false; cells.len()];
    let mut found = Vec::new();
    for start in 0..cells.len() {
        if cells[start] == 0 || seen[start] {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![start];
        let (mut left, mut top, mut right, mut bottom) = (u32::MAX, u32::MAX, 0, 0);
        let mut pixels = 0;
        while let Some(cell) = stack.pop() {
            let (cx, cy) = (cell as u32 % columns, cell as u32 / columns);
            pixels += cells[cell];
            left = left.min(cx);
            top = top.min(cy);
            right = right.max(cx);
            bottom = bottom.max(cy);
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (i64::from(cx) + dx, i64::from(cy) + dy);
                    if nx < 0 || ny < 0 || nx >= i64::from(columns) || ny >= i64::from(rows) {
                        continue;
                    }
                    let next = (ny * i64::from(columns) + nx) as usize;
                    if cells[next] > 0 && !seen[next] {
                        seen[next] = true;
                        stack.push(next);
                    }
                }
            }
        }
        let (x, y) = (left * CELL, top * CELL);
        found.push(Changed {
            x,
            y,
            width: ((right + 1) * CELL).min(width) - x,
            height: ((bottom + 1) * CELL).min(height) - y,
            pixels,
        });
    }
    found.sort_by(|a, b| b.pixels.cmp(&a.pixels).then((a.y, a.x).cmp(&(b.y, b.x))));
    found.truncate(max);
    found.sort_by_key(|r| (r.y, r.x));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn white(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn sizes_come_from_the_header() {
        let png = encode_png(&white(37, 21)).unwrap();
        assert_eq!(image_size(&png), Some((37, 21)));
        // A minimal JPEG header: SOI, an APP0 segment, then SOF0 claiming
        // 640×360.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x68, 0x02, 0x80, 0x03]);
        assert_eq!(image_size(&jpeg), Some((640, 360)));
        // A real one.
        let mut real = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut real, 80)
            .encode_image(&image::RgbImage::new(64, 48))
            .unwrap();
        assert_eq!(image_size(&real), Some((64, 48)));
        assert_eq!(image_size(b"nope"), None);
        assert_eq!(image_size(&[0xFF, 0xD8, 0xFF]), None);
    }

    #[test]
    fn the_same_picture_has_no_changes() {
        let image = white(40, 30);
        let diff = compare(&image, &image, 0.1, MAX_REGIONS).unwrap();
        assert_eq!(diff.changed, 0);
        assert!(diff.regions.is_empty());
        assert!(
            diff.image
                .pixels()
                .all(|p| p.0[3] == 255 && p.0[0] > 220 && p.0[0] == p.0[1] && p.0[1] == p.0[2])
        );
    }

    #[test]
    fn changed_pixels_are_counted_and_red() {
        let before = white(10, 10);
        let mut after = before.clone();
        after.put_pixel(2, 3, Rgba([0, 0, 0, 255]));
        after.put_pixel(7, 7, Rgba([0, 0, 0, 255]));
        let diff = compare(&before, &after, 0.1, MAX_REGIONS).unwrap();
        assert_eq!(diff.changed, 2);
        assert_eq!(diff.image.get_pixel(2, 3).0, [255, 0, 0, 255]);
        assert_eq!(diff.image.get_pixel(7, 7).0, [255, 0, 0, 255]);
        assert_ne!(diff.image.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn the_threshold_is_a_colour_tolerance() {
        let before = white(4, 4);
        let mut after = before.clone();
        after.put_pixel(1, 1, Rgba([250, 250, 250, 255]));
        assert_eq!(compare(&before, &after, 0.1, 20).unwrap().changed, 0);
        assert_eq!(compare(&before, &after, 0.0, 20).unwrap().changed, 1);
    }

    #[test]
    fn sizes_must_match() {
        let err = compare(&white(4, 4), &white(4, 5), 0.1, 20).err().unwrap();
        assert!(err.contains("baseline 4×4, current 4×5"), "{err}");
    }

    #[test]
    fn pngs_round_trip_exactly() {
        let mut image = RgbaImage::new(3, 2);
        image.put_pixel(0, 0, Rgba([1, 2, 3, 4]));
        image.put_pixel(2, 1, Rgba([200, 100, 50, 0]));
        image.put_pixel(1, 1, Rgba([9, 8, 7, 255]));
        let back = decode_png(&encode_png(&image).unwrap()).unwrap();
        assert_eq!(back, image);
        assert!(
            decode_png(b"not a png")
                .unwrap_err()
                .contains("must be a PNG")
        );
    }

    #[test]
    fn changes_are_gathered_in_regions() {
        let before = white(100, 60);
        let mut after = before.clone();
        for y in 5..15 {
            for x in 5..15 {
                after.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        for y in 50..54 {
            for x in 60..90 {
                after.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let diff = compare(&before, &after, 0.1, MAX_REGIONS).unwrap();
        assert_eq!(diff.changed, 220);
        assert_eq!(
            diff.regions,
            [
                Changed {
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 16,
                    pixels: 100
                },
                Changed {
                    x: 48,
                    y: 48,
                    width: 48,
                    height: 12,
                    pixels: 120
                }
            ]
        );
        let one = compare(&before, &after, 0.1, 1).unwrap();
        assert_eq!(one.regions.len(), 1);
        assert_eq!(one.regions[0].pixels, 120);
    }
}

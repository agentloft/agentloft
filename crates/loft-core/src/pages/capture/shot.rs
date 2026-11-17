//! `screenshot` with options, and `screenshot_diff`.
//!
//! A screenshot is of the viewport (what it always was), the whole page,
//! an element or a rectangle, at device or CSS scale, as PNG or JPEG. It
//! is kept within sizes an agent can take in, made smaller with a note
//! when it isn't, and taken in strips when Chrome can't take it at once.
//! Every PNG taken is kept (in memory, per tab) to compare the next one
//! with, and may be kept under a name as a baseline.

use super::super::automation::{Agent, In};
use super::super::lock;
use super::labels;
use super::pixeldiff::{self, MAX_PIXELS, MAX_REGIONS, image_size};
use super::{PageCapture, save};
use base64::Engine as _;
use image::RgbaImage;
use loft_page_script::{
    BrowserError, DiffOptions, EVAL_TIMEOUT, ImageFormat, Region, SCRIPT_TIMEOUT,
    ScreenshotOptions, ShotScale,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;

/// The longest side of an image returned, and of one saved.
const INLINE_SIDE: u32 = 8000;
const SAVED_SIDE: u32 = 16_384;
/// The most bytes of an image returned.
const INLINE_BYTES: usize = 3_750_000;
/// Taken again smaller at most this many times to fit `INLINE_BYTES`.
const BYTE_RETRIES: usize = 3;
/// The tallest strip, in image pixels, when a picture is taken in strips.
const STRIP: f64 = 4096.0;
/// Named baselines a tab keeps.
const BASELINES: usize = 8;
const DEFAULT_THRESHOLD: f64 = 0.1;
const DEFAULT_QUALITY: u8 = 80;
/// How far (CSS px) an area may poke out of the viewport and still count
/// as inside it.
const SLACK: f64 = 0.5;

/// A screenshot kept to compare later: the image, the region as it was
/// asked (an element is found again), and the longest side it was taken
/// within.
#[derive(Clone)]
struct Baseline {
    png: Arc<Vec<u8>>,
    region: Region,
    limit: u32,
}

/// Screenshots kept to compare later: the tab's last one, and up to eight
/// by name (the oldest goes first).
#[derive(Default)]
pub(super) struct Baselines {
    last: Option<Baseline>,
    named: VecDeque<(String, Baseline)>,
}

impl Baselines {
    fn name(&mut self, name: &str, baseline: Baseline) {
        self.named.retain(|(n, _)| n != name);
        self.named.push_back((name.to_owned(), baseline));
        while self.named.len() > BASELINES {
            self.named.pop_front();
        }
    }

    fn named(&self, name: &str) -> Option<&Baseline> {
        self.named.iter().find(|(n, _)| n == name).map(|(_, b)| b)
    }

    fn names(&self) -> Vec<&str> {
        self.named.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Keep `baseline` as the last screenshot, and under `name` if given.
    fn keep(&mut self, name: Option<&str>, baseline: Baseline) {
        if let Some(name) = name {
            self.name(name, baseline.clone());
        }
        self.last = Some(baseline);
    }
}

fn refuse(message: impl Into<String>) -> BrowserError {
    BrowserError::Action(message.into())
}

/// One area at most, and a clip that is a real rectangle.
fn check_region(region: &Region) -> Result<(), BrowserError> {
    if region.ref_id.is_some() && region.selector.is_some() {
        return Err(refuse("use either ref or selector, not both"));
    }
    let element = region.ref_id.is_some() || region.selector.is_some();
    let areas = [region.full_page, element, region.clip.is_some()];
    if areas.iter().filter(|a| **a).count() > 1 {
        return Err(refuse(
            "use only one of full_page, an element (ref or selector) and clip",
        ));
    }
    if let Some(clip) = region.clip {
        let finite = [clip.x, clip.y, clip.width, clip.height]
            .iter()
            .all(|n| n.is_finite());
        if !finite || clip.width <= 0.0 || clip.height <= 0.0 {
            return Err(refuse(
                "clip needs numbers x, y and a positive width and height",
            ));
        }
    }
    Ok(())
}

fn is_viewport(region: &Region) -> bool {
    !region.full_page
        && region.ref_id.is_none()
        && region.selector.is_none()
        && region.clip.is_none()
}

fn round3(n: f64) -> f64 {
    (n * 1000.0).round() / 1000.0
}

/// The viewport, as `Page.getLayoutMetrics` says, and the page's density.
#[derive(Debug, Clone, Copy)]
struct Viewport {
    page_x: f64,
    page_y: f64,
    width: f64,
    height: f64,
    content_width: f64,
    content_height: f64,
    dpr: f64,
}

impl Viewport {
    fn of(metrics: &Value, dpr: f64) -> Self {
        let visual = metrics
            .get("cssVisualViewport")
            .or_else(|| metrics.get("visualViewport"))
            .unwrap_or(&Value::Null);
        let number = |v: &Value, key: &str, default: f64| v[key].as_f64().unwrap_or(default);
        let width = number(visual, "clientWidth", 1280.0);
        let height = number(visual, "clientHeight", 800.0);
        let content = metrics
            .get("cssContentSize")
            .or_else(|| metrics.get("contentSize"))
            .unwrap_or(&Value::Null);
        Self {
            page_x: number(visual, "pageX", 0.0),
            page_y: number(visual, "pageY", 0.0),
            width,
            height,
            content_width: number(content, "width", width).max(1.0),
            content_height: number(content, "height", height).max(1.0),
            dpr: if dpr.is_finite() && dpr > 0.0 {
                dpr
            } else {
                1.0
            },
        }
    }
}

/// What a picture is of, in the document's CSS pixels.
#[derive(Debug, Clone, Copy)]
struct Area {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    /// The viewport as it shows, nothing else.
    viewport: bool,
    /// Reaches outside the viewport: Chrome must lay out beyond it.
    beyond: bool,
}

impl Area {
    fn viewport(v: &Viewport) -> Self {
        Self {
            x: v.page_x,
            y: v.page_y,
            width: v.width,
            height: v.height,
            viewport: true,
            beyond: false,
        }
    }

    fn full_page(v: &Viewport) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: v.content_width,
            height: v.content_height,
            viewport: false,
            beyond: v.content_width > v.width + SLACK
                || v.content_height > v.height + SLACK
                || v.page_x > SLACK
                || v.page_y > SLACK,
        }
    }

    /// A rectangle of the viewport (as `getBoundingClientRect` gives).
    fn clip(v: &Viewport, x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x: x + v.page_x,
            y: y + v.page_y,
            width,
            height,
            viewport: false,
            beyond: x < -SLACK
                || y < -SLACK
                || x + width > v.width + SLACK
                || y + height > v.height + SLACK,
        }
    }
}

/// How a picture is asked for.
#[derive(Debug, Clone, Copy)]
struct Request {
    format: ImageFormat,
    quality: u8,
    css: bool,
    /// The longest side, in image pixels.
    limit: u32,
    saving: bool,
}

/// A picture taken.
struct Shot {
    bytes: Vec<u8>,
    format: ImageFormat,
    width: u32,
    height: u32,
    /// Image pixels per CSS pixel of the area.
    scale: f64,
    notes: Vec<String>,
    /// The longest side it was taken within.
    limit: u32,
}

const SMALLER: &str = "made smaller";

/// The image in a `Page.captureScreenshot` reply, and its size.
fn picture(reply: &Value) -> Result<(Vec<u8>, (u32, u32)), BrowserError> {
    let data = reply["data"]
        .as_str()
        .ok_or_else(|| refuse("screenshot failed: no picture"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| refuse(format!("screenshot failed: {e}")))?;
    let size = image_size(&bytes)
        .ok_or_else(|| refuse("screenshot failed: the browser's picture is unreadable"))?;
    Ok((bytes, size))
}

fn stitch(tiles: &[RgbaImage]) -> Result<RgbaImage, String> {
    let width = tiles.first().map_or(0, RgbaImage::width);
    if tiles.iter().any(|t| t.width() != width) {
        return Err("tiles came back with different widths".into());
    }
    let height: u32 = tiles.iter().map(RgbaImage::height).sum();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("too large to stitch (over 16 megapixels)".into());
    }
    let mut whole = RgbaImage::new(width, height);
    let mut y = 0;
    for tile in tiles {
        image::imageops::replace(&mut whole, tile, 0, i64::from(y));
        y += tile.height();
    }
    Ok(whole)
}

impl Agent<'_> {
    /// `screenshot`.
    pub(in crate::pages) async fn screenshot(
        &self,
        options: ScreenshotOptions,
    ) -> Result<Value, BrowserError> {
        let asked = options.format.unwrap_or(ImageFormat::Png);
        let region = &options.region;
        if options.labels && (asked != ImageFormat::Png || !is_viewport(region)) {
            return Err(refuse(
                "labels work on screenshots of the viewport as PNG only",
            ));
        }
        if options.baseline.is_some() && asked != ImageFormat::Png {
            return Err(refuse("a baseline is a PNG: leave format out, or png"));
        }
        check_region(region)?;

        // The elements to number, found before the picture is taken.
        let found = match options.labels {
            true => {
                let script = labels::script(labels::how_many(options.max_labels));
                let reply = self
                    .evaluate(
                        &script,
                        In::Helpers,
                        SCRIPT_TIMEOUT,
                        "finding the elements to label",
                    )
                    .await?
                    .ok_or_else(|| {
                        BrowserError::Script("the labels script returned nothing".into())
                    })?;
                Some(labels::parse(&reply).map_err(BrowserError::Script)?)
            }
            false => None,
        };

        let (area, viewport) = self.area(region).await?;
        let mut request = Request {
            format: asked,
            quality: options.quality.map_or(DEFAULT_QUALITY, |q| q.min(100)),
            css: region.scale == Some(ShotScale::Css),
            limit: if options.save {
                SAVED_SIDE
            } else {
                INLINE_SIDE
            },
            saving: options.save,
        };
        let mut shot = self.shoot(&area, &viewport, &request).await?;
        if !options.save {
            for _ in 0..BYTE_RETRIES {
                if shot.bytes.len() <= INLINE_BYTES {
                    break;
                }
                let longest = f64::from(shot.width.max(shot.height));
                let fits = (INLINE_BYTES as f64 / shot.bytes.len() as f64).sqrt();
                request.limit = ((longest * fits * 0.9).floor() as u32).max(64);
                shot = self.shoot(&area, &viewport, &request).await?;
                shot.notes.retain(|n| !n.starts_with(SMALLER));
                shot.notes.push(
                    "made smaller to fit 3 MB (save it, or take a smaller region, for every detail)"
                        .into(),
                );
            }
        }
        if asked == ImageFormat::Png && options.quality.is_some() {
            shot.notes
                .push("quality is for jpeg only; it was ignored".into());
        }

        // Kept unlabelled, to compare later.
        if shot.format == ImageFormat::Png {
            let kept = Baseline {
                png: Arc::new(shot.bytes.clone()),
                region: region.clone(),
                limit: shot.limit,
            };
            lock(&self.captures().shots).keep(options.baseline.as_deref(), kept);
        }

        if let Some(found) = &found {
            let (png, scale, labels) = (std::mem::take(&mut shot.bytes), shot.scale, found.clone());
            shot.bytes = tokio::task::spawn_blocking(move || {
                let mut image = pixeldiff::decode_png(&png)?;
                labels::draw(&mut image, &labels, scale);
                pixeldiff::encode_png(&image)
            })
            .await
            .map_err(|e| refuse(format!("drawing the labels: {e}")))?
            .map_err(|e| refuse(format!("drawing the labels: {e}")))?;
        }

        let (mime, ext) = match shot.format {
            ImageFormat::Png => ("image/png", "png"),
            ImageFormat::Jpeg => ("image/jpeg", "jpg"),
        };
        let mut result = serde_json::Map::new();
        result.insert("mime_type".into(), json!(mime));
        result.insert("width".into(), json!(shot.width));
        result.insert("height".into(), json!(shot.height));
        result.insert("scale".into(), json!(shot.scale));
        if asked != ImageFormat::Png || shot.format != asked {
            result.insert("format".into(), json!(ext.replace("jpg", "jpeg")));
        }
        if !shot.notes.is_empty() {
            result.insert("note".into(), json!(shot.notes.join("; ")));
        }
        if let Some(found) = found {
            result.insert("labels".into(), Value::Array(found));
        }
        if let Some(name) = &options.baseline {
            result.insert("baseline".into(), json!(name));
        }
        if options.save {
            let path = save(self.page, "screenshot", ext, &shot.bytes)?;
            result.insert("path".into(), json!(path));
            result.insert("bytes".into(), json!(shot.bytes.len()));
        } else {
            result.insert(
                "data".into(),
                json!(base64::engine::general_purpose::STANDARD.encode(&shot.bytes)),
            );
        }
        Ok(Value::Object(result))
    }

    /// `screenshot_diff`.
    pub(super) async fn screenshot_diff(
        &self,
        state: &PageCapture,
        options: DiffOptions,
    ) -> Result<Value, BrowserError> {
        let threshold = options.threshold.unwrap_or(DEFAULT_THRESHOLD);
        if !(0.0..=1.0).contains(&threshold) {
            return Err(refuse("threshold must be from 0 to 1"));
        }
        let baseline = {
            let shots = lock(&state.shots);
            match &options.baseline {
                Some(name) => shots.named(name).cloned().ok_or_else(|| {
                    let names = shots.names();
                    let there = if names.is_empty() {
                        String::new()
                    } else {
                        format!(" (there are {})", names.join(", "))
                    };
                    refuse(format!(
                        "no baseline named {name:?} in this tab{there}; take one first: screenshot with baseline {name:?}"
                    ))
                })?,
                None => shots.last.clone().ok_or_else(|| {
                    refuse(
                        "no screenshot to compare with yet: take one first (screenshot, or \
                         screenshot_diff keeps the one it takes), change the page, then diff",
                    )
                })?,
            }
        };
        let region = if options.region.is_default() {
            baseline.region.clone()
        } else {
            options.region.clone()
        };
        check_region(&region)?;
        let (area, viewport) = self.area(&region).await?;
        let request = Request {
            format: ImageFormat::Png,
            quality: DEFAULT_QUALITY,
            css: region.scale == Some(ShotScale::Css),
            limit: baseline.limit,
            saving: false,
        };
        let shot = self.shoot(&area, &viewport, &request).await?;
        if shot.format != ImageFormat::Png {
            return Err(refuse(
                "screenshot failed: the browser's picture isn't a PNG",
            ));
        }
        let fresh = Arc::new(shot.bytes);
        let (before, after) = (baseline.png.clone(), fresh.clone());
        let compared = tokio::task::spawn_blocking(move || {
            let before = pixeldiff::decode_png(&before)?;
            let after = pixeldiff::decode_png(&after)?;
            let diff = pixeldiff::compare(&before, &after, threshold, MAX_REGIONS)?;
            let png = match diff.changed {
                0 => None,
                _ => Some(pixeldiff::encode_png(&diff.image)?),
            };
            Ok::<_, String>((diff.changed, diff.regions, png, before.dimensions()))
        })
        .await
        .map_err(|e| refuse(format!("comparing the screenshots: {e}")))?
        .map_err(BrowserError::Action)?;
        let (changed, regions, png, (width, height)) = compared;

        // The fresh one is the last now: diffs in a row compare each step
        // with the one before.
        {
            let kept = Baseline {
                png: fresh,
                region,
                limit: shot.limit,
            };
            let name = options.baseline.as_deref().filter(|_| options.update);
            lock(&state.shots).keep(name, kept);
        }

        let total = u64::from(width) * u64::from(height);
        let scale = shot.scale;
        let mut result = json!({
            "matches": changed == 0,
            "changed_pixels": changed,
            "total_pixels": total,
            "changed_ratio": if total > 0 { changed as f64 / total as f64 } else { 0.0 },
            "width": width,
            "height": height,
            "scale": scale,
            "threshold": threshold,
            "baseline": options.baseline.as_deref().unwrap_or("the tab's last screenshot"),
        });
        let Some(png) = png else {
            return Ok(result);
        };
        let css = |n: u32| round3(f64::from(n) / scale);
        result["regions"] = regions
            .iter()
            .map(|r| {
                json!({
                    "x": css(r.x),
                    "y": css(r.y),
                    "width": css(r.width),
                    "height": css(r.height),
                    "changed_pixels": r.pixels,
                })
            })
            .collect();
        let too_big = png.len() > INLINE_BYTES || width.max(height) > INLINE_SIDE;
        if options.save || too_big {
            let path = save(self.page, "diff", "png", &png)?;
            result["path"] = json!(path);
            if !options.save {
                result["note"] = json!("the diff image is too big to return, so it was saved");
            }
        } else {
            result["mime_type"] = json!("image/png");
            result["data"] = json!(base64::engine::general_purpose::STANDARD.encode(&png));
        }
        Ok(result)
    }

    /// Where a region is now, and the viewport it was measured in.
    async fn area(&self, region: &Region) -> Result<(Area, Viewport), BrowserError> {
        // An element first: finding it may scroll it into view.
        let element = match (region.ref_id, &region.selector) {
            (None, None) => None,
            (ref_id, selector) => Some(self.element_box(ref_id, selector.as_deref()).await?),
        };
        let metrics = self
            .send(
                "Page.getLayoutMetrics",
                json!({}),
                SCRIPT_TIMEOUT,
                "measuring the page",
            )
            .await?;
        let dpr = f64::from(lock(&self.page.state).info.scale);
        let viewport = Viewport::of(&metrics, dpr);
        let area = if let Some([x, y, width, height]) = element {
            Area::clip(&viewport, x, y, width, height)
        } else if let Some(clip) = region.clip {
            Area::clip(&viewport, clip.x, clip.y, clip.width, clip.height)
        } else if region.full_page {
            Area::full_page(&viewport)
        } else {
            Area::viewport(&viewport)
        };
        Ok((area, viewport))
    }

    /// An element's box in the viewport (CSS pixels), scrolled into view
    /// first if it is partly out of it but would fit.
    async fn element_box(
        &self,
        ref_id: Option<u32>,
        selector: Option<&str>,
    ) -> Result<[f64; 4], BrowserError> {
        let args = json!({ "ref": ref_id, "selector": selector });
        let body = format!("const ARGS = {args};\n{ELEMENT_JS}");
        let reply = self
            .evaluate(&body, In::Helpers, SCRIPT_TIMEOUT, "finding the element")
            .await?
            .unwrap_or_default();
        let found: Value = serde_json::from_str(&reply)
            .map_err(|_| BrowserError::Script("unreadable element box".into()))?;
        let named = || match (ref_id, selector) {
            (Some(id), _) => format!("[ref={id}]"),
            (None, selector) => format!("the element {:?}", selector.unwrap_or_default()),
        };
        match found["error"].as_str() {
            None => {}
            Some("ref") => return Err(BrowserError::RefNotFound(ref_id.unwrap_or(0))),
            Some("none") => {
                return Err(refuse(format!(
                    "no element matches {:?}",
                    selector.unwrap_or_default()
                )));
            }
            Some("syntax") => {
                return Err(refuse(format!(
                    "{:?} isn't a valid selector",
                    selector.unwrap_or_default()
                )));
            }
            Some("size") => return Err(refuse(format!("{} has no visible size", named()))),
            Some(_) => return Err(BrowserError::Script("unreadable element box".into())),
        }
        let number = |key: &str| found[key].as_f64().filter(|n| n.is_finite());
        match (number("x"), number("y"), number("width"), number("height")) {
            (Some(x), Some(y), Some(w), Some(h)) => Ok([x, y, w, h]),
            _ => Err(BrowserError::Script("unreadable element box".into())),
        }
    }

    /// Take the area as asked, shrunk to fit the request's longest side,
    /// in strips if Chrome can't take it at once.
    async fn shoot(
        &self,
        area: &Area,
        viewport: &Viewport,
        request: &Request,
    ) -> Result<Shot, BrowserError> {
        let per_css = if request.css { 1.0 } else { viewport.dpr };
        let whole = (
            (area.width * per_css).round(),
            (area.height * per_css).round(),
        );
        let longest = whole.0.max(whole.1);
        let limit = f64::from(request.limit);
        let shrink = if longest > limit {
            limit / longest
        } else {
            1.0
        };
        let mut notes = Vec::new();
        if shrink < 1.0 {
            let mut note = format!(
                "{SMALLER} ({:.0}%) to fit {} px a side (the whole would be {}×{} px); capture a clip or an element for every detail",
                shrink * 100.0,
                request.limit,
                whole.0,
                whole.1
            );
            if !request.saving {
                note.push_str(&format!(", or save it for up to {SAVED_SIDE} px"));
            }
            notes.push(note);
        }
        let clip_scale = per_css / viewport.dpr * shrink;
        let plain = area.viewport && shrink == 1.0 && !request.css;
        let mut params = match request.format {
            ImageFormat::Png => json!({ "format": "png" }),
            ImageFormat::Jpeg => json!({ "format": "jpeg", "quality": request.quality }),
        };
        if !plain {
            params["clip"] = json!({
                "x": area.x, "y": area.y, "width": area.width, "height": area.height,
                "scale": clip_scale,
            });
            params["fromSurface"] = json!(true);
            if area.beyond {
                params["captureBeyondViewport"] = json!(true);
            }
        }
        let taken = {
            let _shaping = self.page.shaping.lock().await;
            self.send(
                "Page.captureScreenshot",
                params,
                EVAL_TIMEOUT,
                "taking the screenshot",
            )
            .await
        };
        let (bytes, format, (width, height)) = match taken {
            Ok(reply) => {
                let (bytes, size) = picture(&reply)?;
                (bytes, request.format, size)
            }
            Err(BrowserError::Action(chrome))
                if area.beyond && chrome.contains("Unable to capture screenshot") =>
            {
                let size = ((whole.0 * shrink).round(), (whole.1 * shrink).round());
                let bytes = self
                    .in_strips(area, per_css * shrink, clip_scale)
                    .await
                    .map_err(|tiles| {
                        refuse(format!(
                            "Chrome couldn't capture this {}×{} px screenshot ({chrome}; in tiles: {tiles}); capture a clip or an element instead",
                            size.0, size.1
                        ))
                    })?;
                if request.format != ImageFormat::Png {
                    notes.push("taken in tiles and returned as PNG".into());
                }
                let size = image_size(&bytes).ok_or_else(|| {
                    refuse("screenshot failed: the browser's picture is unreadable")
                })?;
                (bytes, ImageFormat::Png, size)
            }
            Err(err) => return Err(err),
        };
        Ok(Shot {
            bytes,
            format,
            width,
            height,
            scale: round3(f64::from(width) / area.width),
            notes,
            limit: request.limit,
        })
    }

    /// The area in horizontal strips, stitched into one PNG.
    async fn in_strips(
        &self,
        area: &Area,
        per_css: f64,
        clip_scale: f64,
    ) -> Result<Vec<u8>, String> {
        let expected = (area.width * per_css).round() * (area.height * per_css).round();
        if expected > MAX_PIXELS as f64 {
            return Err("too large to stitch (over 16 megapixels)".into());
        }
        let strip = (STRIP / per_css).floor().max(1.0);
        let end = area.y + area.height;
        let mut tiles = Vec::new();
        let mut y = area.y;
        while end - y > 1e-6 {
            let height = strip.min(end - y);
            let params = json!({
                "format": "png",
                "clip": { "x": area.x, "y": y, "width": area.width, "height": height, "scale": clip_scale },
                "fromSurface": true,
                "captureBeyondViewport": true,
            });
            let reply = {
                let _shaping = self.page.shaping.lock().await;
                self.send(
                    "Page.captureScreenshot",
                    params,
                    EVAL_TIMEOUT,
                    "taking a strip of the screenshot",
                )
                .await
                .map_err(|e| e.to_string())?
            };
            let (bytes, _) = picture(&reply).map_err(|e| e.to_string())?;
            tiles.push(bytes);
            y += height;
        }
        tokio::task::spawn_blocking(move || {
            let decoded = tiles
                .iter()
                .map(|bytes| {
                    image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
                        .map(|i| i.to_rgba8())
                        .map_err(|e| format!("a strip couldn't be read: {e}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            pixeldiff::encode_png(&stitch(&decoded)?)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// Finds an element by ref or selector in the helpers' world and measures
/// it in the top viewport, through any same-origin frames it is in.
const ELEMENT_JS: &str = r#"
const find = () => {
  if (ARGS.ref != null) {
    const agent = globalThis.__loftAgent;
    const known = agent && agent.refs && agent.refs.get(ARGS.ref);
    const el = known && known.deref();
    return el && el.isConnected ? { el } : { error: 'ref' };
  }
  const steps = String(ARGS.selector || '').split('>>').map((s) => s.trim()).filter(Boolean);
  if (!steps.length) return { error: 'none' };
  let scope = document;
  let el = null;
  for (const step of steps) {
    if (el) {
      scope = el.shadowRoot;
      if (!scope) return { error: 'none' };
    }
    try {
      el = scope.querySelector(step);
    } catch (e) {
      if (e && e.name === 'SyntaxError') return { error: 'syntax' };
      throw e;
    }
    if (!el) return { error: 'none' };
  }
  return { el };
};
const measure = (el) => {
  const r = el.getBoundingClientRect();
  let x = r.left;
  let y = r.top;
  let view = el.ownerDocument.defaultView;
  while (view && view !== window && view.frameElement) {
    const frame = view.frameElement;
    const fr = frame.getBoundingClientRect();
    x += fr.left + frame.clientLeft;
    y += fr.top + frame.clientTop;
    view = frame.ownerDocument.defaultView;
  }
  return { x, y, width: r.width, height: r.height };
};
const found = find();
if (found.error) return JSON.stringify({ error: found.error });
let box = measure(found.el);
if (!(box.width > 0) || !(box.height > 0)) return JSON.stringify({ error: 'size' });
const W = innerWidth;
const H = innerHeight;
const out = box.x < -0.5 || box.y < -0.5 || box.x + box.width > W + 0.5 || box.y + box.height > H + 0.5;
if (out && box.width <= W && box.height <= H) {
  found.el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  box = measure(found.el);
}
return JSON.stringify(box);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use loft_page_script::Clip;

    #[test]
    fn regions_are_checked() {
        assert!(check_region(&Region::default()).is_ok());
        let both = Region {
            ref_id: Some(1),
            selector: Some("#a".into()),
            ..Region::default()
        };
        assert_eq!(
            check_region(&both).unwrap_err().to_string(),
            "use either ref or selector, not both"
        );
        let two = Region {
            full_page: true,
            ref_id: Some(1),
            ..Region::default()
        };
        assert!(
            check_region(&two)
                .unwrap_err()
                .to_string()
                .starts_with("use only one of")
        );
        let flat = Region {
            clip: Some(Clip {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 10.0,
            }),
            ..Region::default()
        };
        assert!(
            check_region(&flat)
                .unwrap_err()
                .to_string()
                .starts_with("clip needs numbers")
        );
    }

    fn kept(n: u8) -> Baseline {
        Baseline {
            png: Arc::new(vec![n]),
            region: Region::default(),
            limit: INLINE_SIDE,
        }
    }

    #[test]
    fn baselines_keep_the_newest_eight() {
        let mut shots = Baselines::default();
        assert!(shots.last.is_none() && shots.names().is_empty());
        shots.keep(None, kept(0));
        assert_eq!(*shots.last.as_ref().unwrap().png, [0]);
        for n in 1..=10 {
            shots.keep(Some(&format!("b{n}")), kept(n));
        }
        assert_eq!(
            shots.names(),
            ["b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10"]
        );
        assert!(shots.named("b2").is_none());
        assert_eq!(
            *shots.last.as_ref().unwrap().png,
            [10],
            "the last is the newest"
        );
        // Named again: replaced, and the newest.
        shots.keep(Some("b4"), kept(44));
        assert_eq!(*shots.named("b4").unwrap().png, [44]);
        assert_eq!(shots.names().last(), Some(&"b4"));
        assert_eq!(shots.names().len(), 8);
    }

    #[test]
    fn areas_know_when_they_reach_out_of_the_viewport() {
        let metrics = json!({
            "cssVisualViewport": { "pageX": 0, "pageY": 100, "clientWidth": 1280, "clientHeight": 800 },
            "cssContentSize": { "width": 1280, "height": 3000 },
        });
        let v = Viewport::of(&metrics, 2.0);
        assert_eq!(v.dpr, 2.0);
        assert!(Area::full_page(&v).beyond);
        let inside = Area::clip(&v, 10.0, 10.0, 100.0, 100.0);
        assert!(!inside.beyond);
        assert_eq!(inside.y, 110.0, "in document coordinates");
        assert!(Area::clip(&v, 10.0, 750.0, 100.0, 100.0).beyond);
        let fallback = Viewport::of(&json!({}), f64::NAN);
        assert_eq!(
            (fallback.width, fallback.height, fallback.dpr),
            (1280.0, 800.0, 1.0)
        );
    }

    /// Big pictures are made smaller with a note; elements in shadow roots
    /// and below the fold are found; labels name shadow elements so they
    /// can be found again. `cargo test -p loft-core pages::capture -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts a real browser"]
    async fn live_big_pictures_shadow_roots_and_the_fold() {
        use crate::pages::{AGENT_PAGE_SIZE, Pages, Which};
        use loft_page_script::BrowserCommand;
        let dir = tempfile::tempdir().unwrap();
        let (bus, _heard) = tokio::sync::broadcast::channel(64);
        let pages = Pages::default();
        let (width, height, scale) = AGENT_PAGE_SIZE;
        let page = r#"<!doctype html><meta charset=utf-8><body style='margin:0'>
            <canvas id=noise width=1280 height=1200 style='display:block'></canvas>
            <div style='height:9000px'></div>
            <div id=low style='width:300px;height:50px;background:#0a0'></div>
            <div id=host></div>
            <script>
              const c = document.getElementById('noise').getContext('2d');
              const d = c.createImageData(1280, 1200);
              for (let i = 0; i < d.data.length; i++) d.data[i] = (i % 4 === 3) ? 255 : Math.random() * 256;
              c.putImageData(d, 0, 0);
              const root = document.getElementById('host').attachShadow({ mode: 'open' });
              root.innerHTML = "<button style='width:90px;height:30px'>Deep</button>";
            </script>"#;
        let file = dir.path().join("big.html");
        std::fs::write(&file, page).unwrap();
        let url = format!("file://{}", file.display());
        let info = pages
            .open(
                loft_protocol::ThreadId::new(),
                Which::Current,
                None,
                width,
                height,
                scale,
                None,
                &dir.path().join("profile"),
                bus,
            )
            .await
            .unwrap();
        let drive = async |command: Value| {
            let command: BrowserCommand = serde_json::from_value(command).unwrap();
            pages
                .drive(&info.id, command, loft_page_script::DEFAULT_SEARCH_TEMPLATE)
                .await
        };
        drive(json!({ "action": "navigate", "url": url }))
            .await
            .unwrap();

        // Over 8000 px a side: smaller, and said so.
        let full = drive(json!({ "action": "screenshot", "full_page": true }))
            .await
            .unwrap();
        let note = full["note"].as_str().unwrap();
        assert!(note.contains("to fit") || note.contains("3 MB"), "{note}");
        assert!(
            full["width"]
                .as_u64()
                .unwrap()
                .max(full["height"].as_u64().unwrap())
                <= 8000
        );
        let data = base64::engine::general_purpose::STANDARD
            .decode(full["data"].as_str().unwrap())
            .unwrap();
        assert!(data.len() <= INLINE_BYTES, "{}", data.len());
        // Noise doesn't compress: taken again smaller to fit 3 MB.
        let noisy = drive(json!({ "action": "screenshot", "clip": { "x": 0, "y": 0, "width": 1280, "height": 800 } }))
            .await
            .unwrap();
        assert!(
            noisy["note"].as_str().unwrap().contains("3 MB"),
            "{}",
            noisy["note"]
        );

        // Below the fold: scrolled into view, then taken.
        let low = drive(json!({ "action": "screenshot", "selector": "#low", "scale": "css" }))
            .await
            .unwrap();
        assert_eq!((&low["width"], &low["height"]), (&json!(300), &json!(50)));
        // Inside a shadow root, and labelled so it can be found again.
        let labelled = drive(json!({ "action": "screenshot", "labels": true, "scale": "css" }))
            .await
            .unwrap();
        let deep = labelled["labels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["text"] == "Deep")
            .unwrap_or_else(|| panic!("{}", labelled["labels"]))
            .clone();
        assert_eq!(deep["selector"], "#host >> button:nth-of-type(1)");
        let again =
            drive(json!({ "action": "screenshot", "selector": deep["selector"], "scale": "css" }))
                .await
                .unwrap();
        assert_eq!(
            (&again["width"], &again["height"]),
            (&json!(90), &json!(30))
        );
        pages.close(&info.id).await.unwrap();
        pages.shutdown().await;
    }

    #[test]
    fn strips_stitch_top_to_bottom() {
        let top = RgbaImage::from_pixel(4, 2, image::Rgba([1, 1, 1, 255]));
        let bottom = RgbaImage::from_pixel(4, 3, image::Rgba([2, 2, 2, 255]));
        let whole = stitch(&[top, bottom.clone()]).unwrap();
        assert_eq!(whole.dimensions(), (4, 5));
        assert_eq!(whole.get_pixel(0, 4).0, [2, 2, 2, 255]);
        let narrow = RgbaImage::new(3, 1);
        assert_eq!(
            stitch(&[bottom, narrow]).unwrap_err(),
            "tiles came back with different widths"
        );
    }
}

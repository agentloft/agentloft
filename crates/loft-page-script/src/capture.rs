//! What the capture commands take (ADR-0017 §8): screenshots of a region,
//! screenshot diffs, a tab's network traffic, its console, a recording of
//! it, and how it presents itself (emulation).
//!
//! Every field is optional and left out of the wire form when unset, so
//! `{"action": "screenshot"}` is still the whole viewport as a PNG.

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn is_false(b: &bool) -> bool {
    !*b
}

/// A rectangle of the viewport, in CSS pixels (as `getBoundingClientRect`
/// gives them).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    #[serde(alias = "jpg")]
    Jpeg,
}

/// How many image pixels a CSS pixel gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShotScale {
    /// One image pixel per CSS pixel: a point in the image is the point
    /// clicks use.
    Css,
    /// Every device pixel (twice as many each way on a dense screen): what
    /// a screenshot always was.
    Device,
}

/// Which part of the page a screenshot shows. At most one of `full_page`,
/// an element (`ref_id` or `selector`) and `clip`; none is the viewport.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Region {
    #[serde(skip_serializing_if = "is_false")]
    pub full_page: bool,
    /// An element from the latest snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<u32>,
    /// A CSS selector; ` >> ` steps into an open shadow root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<Clip>,
    /// Device pixels unless asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<ShotScale>,
}

impl Region {
    /// Nothing was asked for: the viewport at device scale.
    pub fn is_default(&self) -> bool {
        *self == Region::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotOptions {
    #[serde(flatten)]
    pub region: Region,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ImageFormat>,
    /// JPEG quality, 0 to 100 (80 if not given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    /// Number the interactive elements in view on the picture (a viewport
    /// PNG), and list them with a selector and a box each.
    #[serde(skip_serializing_if = "is_false")]
    pub labels: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_labels: Option<u32>,
    /// Save it to a file (its path is returned) instead of returning it.
    #[serde(skip_serializing_if = "is_false")]
    pub save: bool,
    /// Keep it under this name, as a baseline for `screenshot_diff`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffOptions {
    /// The region of the fresh screenshot; the baseline's when none is
    /// given.
    #[serde(flatten)]
    pub region: Region,
    /// A name given to an earlier screenshot; the tab's last screenshot
    /// when not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// How different a pixel's colour may be and still count as the same,
    /// 0 to 1 (0.1 if not given). Not an allowance for changed area.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Keep the fresh screenshot as the named baseline afterwards.
    #[serde(skip_serializing_if = "is_false")]
    pub update: bool,
    /// Save the diff image to a file instead of returning it.
    #[serde(skip_serializing_if = "is_false")]
    pub save: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkOptions {
    /// Only addresses that contain this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_filter: Option<String>,
    /// Only these DevTools resource types (`Document`, `XHR`, `Fetch`, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resource_types: Vec<String>,
    /// Only these methods (`POST`, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Keep request and response bodies (JSON, form and multipart only);
    /// on unless `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bodies: Option<bool>,
    /// The capture ends itself (and is discarded) after this long.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkStopOptions {
    /// Save the whole capture to a JSON file as well (it is saved anyway
    /// when it is too big to return).
    #[serde(skip_serializing_if = "is_false")]
    pub save: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsoleReadOptions {
    /// Only these kinds: `log`, `info`, `warning`, `error`, `debug`,
    /// `pageerror` (uncaught), … Empty: all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
    /// Only `console.error`, failed `console.assert`, uncaught errors and
    /// the browser's errors.
    #[serde(skip_serializing_if = "is_false")]
    pub errors_only: bool,
    /// Only messages that contain this (any case).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Only messages after this `seq` (an earlier read's `last_seq`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// The newest this many (200 if not given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Forget what was returned; on unless `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clear: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingOptions {
    /// The file's name: `.mkv` (MJPEG, written as it goes), or `.mp4` or
    /// `.webm` (made with ffmpeg when it stops).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Frames a second in the file (30 if not given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<u32>,
    /// It stops itself after this long.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
}

/// How the tab presents itself. `"reset"` puts a setting back; `reset:
/// true` puts back everything an agent set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmulateOptions {
    /// `{width, height, mobile?, user_agent?}` in CSS pixels, or `"reset"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduced_motion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced_colors: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<String>,
    /// `screen` or `print`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// `{latitude, longitude, accuracy?}` or `"reset"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geolocation: Option<Value>,
    /// An IANA time zone (`Europe/Paris`) or `"reset"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// A locale (`de-DE`) or `"reset"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// `"offline"`, `"slow-3g"`, `"fast-3g"`, `"4g"`, `"reset"`, or
    /// `{latency_ms, download_kbps, upload_kbps}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    /// How many times slower the CPU is (1: not throttled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_throttling: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub touch: Option<bool>,
    #[serde(skip_serializing_if = "is_false")]
    pub reset: bool,
}

impl EmulateOptions {
    /// Whether anything at all was asked for.
    pub fn is_empty(&self) -> bool {
        *self == EmulateOptions::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::BrowserCommand;
    use serde_json::json;

    fn round_trip(wire: serde_json::Value) -> BrowserCommand {
        let cmd: BrowserCommand = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&cmd).unwrap(), wire, "{cmd:?}");
        cmd
    }

    #[test]
    fn capture_commands_keep_their_wire_shape() {
        let BrowserCommand::Screenshot(shot) = round_trip(json!({
            "action": "screenshot", "full_page": true, "format": "jpeg", "quality": 60,
            "scale": "css", "save": true, "baseline": "home"
        })) else {
            panic!()
        };
        assert!(shot.region.full_page);
        assert_eq!(shot.quality, Some(60));
        round_trip(json!({ "action": "screenshot", "ref_id": 4 }));
        round_trip(json!({
            "action": "screenshot", "clip": { "x": 1.0, "y": 2.0, "width": 3.0, "height": 4.0 },
            "labels": true, "max_labels": 20
        }));
        round_trip(json!({ "action": "screenshot_diff", "baseline": "home", "threshold": 0.2 }));
        round_trip(json!({ "action": "network_start", "url_filter": "/api", "methods": ["POST"] }));
        round_trip(json!({ "action": "network_status" }));
        round_trip(json!({ "action": "network_stop", "save": true }));
        round_trip(json!({ "action": "network_cancel" }));
        round_trip(json!({ "action": "console_start" }));
        round_trip(json!({ "action": "console_read", "errors_only": true, "since": 3 }));
        round_trip(json!({ "action": "console_stop" }));
        round_trip(json!({ "action": "recording_start", "name": "demo.mkv", "frame_rate": 10 }));
        round_trip(json!({ "action": "recording_status" }));
        round_trip(json!({ "action": "recording_stop" }));
        round_trip(json!({ "action": "recording_cancel" }));
        round_trip(json!({
            "action": "emulate", "viewport": { "width": 390, "height": 844, "mobile": true },
            "color_scheme": "dark", "timezone": "Asia/Tokyo", "network": "slow-3g"
        }));
        // Bare actions take their defaults.
        let BrowserCommand::NetworkStop(stop) =
            serde_json::from_value(json!({ "action": "network_stop" })).unwrap()
        else {
            panic!()
        };
        assert!(!stop.save);
        assert_eq!(
            serde_json::from_value::<BrowserCommand>(
                json!({ "action": "screenshot", "format": "jpg" })
            )
            .unwrap(),
            BrowserCommand::Screenshot(crate::ScreenshotOptions {
                format: Some(crate::ImageFormat::Jpeg),
                ..Default::default()
            })
        );
    }
}

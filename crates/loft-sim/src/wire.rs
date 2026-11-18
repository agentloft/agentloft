//! What the daemon and its simulator helper say to each other.
//!
//! The daemon writes one JSON [`Request`] per line to the helper's stdin.
//! The helper writes messages to its stdout, each a kind byte, a big-endian
//! `u32` length and that many bytes:
//!
//! - [`KIND_EVENT`]: an [`Event`] as JSON;
//! - [`KIND_FRAME`]: a picture of the screen: its width and height (big-endian
//!   `u32`s), then a JPEG;
//! - [`KIND_CAPTURE`]: the answer to [`Command::Capture`]: the request's id
//!   (big-endian `u64`), then a PNG.
//!
//! The helper ends when its stdin does.

use loft_protocol::{DeviceInput, DeviceOrientation};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const KIND_EVENT: u8 = 1;
pub const KIND_FRAME: u8 = 2;
pub const KIND_CAPTURE: u8 = 3;

/// Longer messages are refused (a picture of the largest iPad is a few MB).
pub const MAX_MESSAGE: usize = 64 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Someone is looking (or no one is): pictures come only while on.
    Watch {
        on: bool,
    },
    Input {
        input: DeviceInput,
    },
    /// A PNG of the whole screen as it is turned now.
    Capture,
    /// Turn the device to face this way, as the rotate buttons do.
    Orientation {
        orientation: DeviceOrientation,
    },
    /// Outline every element accessibility knows of in the app in front,
    /// in the pictures and captures (done once the first outlines are
    /// read).
    AccessibilityFrames {
        on: bool,
    },
}

/// The screen as it is turned now: pixels, and pixels per point.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    /// Which way the interface faces (not how the device is held: an
    /// iPhone's home screen stays upright whichever way it is turned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orientation: Option<DeviceOrientation>,
    /// Which of the device's built-in screens is shown, when it has more
    /// than one (a foldable): its screen ID, 1 being its main screen (the
    /// cover).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Attached: pictures and input work from now on.
    Ready { screen: ScreenSize },
    /// The screen turned.
    Size { screen: ScreenSize },
    /// A request was carried out (or wasn't, and why).
    Done {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The helper is stopping (the device shut down, or something broke).
    Ended { reason: String },
}

/// Write one message (`parts` one after the other) in a single write.
pub fn write_message(out: &mut impl Write, kind: u8, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(5 + len);
    buf.push(kind);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    for part in parts {
        buf.extend_from_slice(part);
    }
    out.write_all(&buf)?;
    out.flush()
}

pub fn write_event(out: &mut impl Write, event: &Event) -> io::Result<()> {
    let json = serde_json::to_vec(event).map_err(io::Error::other)?;
    write_message(out, KIND_EVENT, &[&json])
}

/// Read one message: its kind and body. `Ok(None)` at the end of the stream.
pub fn read_message(input: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match input.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::other(format!("message of {len} bytes")));
    }
    let mut body = vec![0; len];
    input.read_exact(&mut body)?;
    Ok(Some((head[0], body)))
}

/// A [`KIND_FRAME`] body: width, height and the JPEG.
pub fn split_frame(body: &[u8]) -> Option<(u32, u32, &[u8])> {
    let width = u32::from_be_bytes(body.get(0..4)?.try_into().ok()?);
    let height = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?);
    Some((width, height, &body[8..]))
}

/// A [`KIND_CAPTURE`] body: the request's id and the PNG.
pub fn split_capture(body: &[u8]) -> Option<(u64, &[u8])> {
    let id = u64::from_be_bytes(body.get(0..8)?.try_into().ok()?);
    Some((id, &body[8..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::TouchPhase;

    #[test]
    fn requests_are_one_json_object() {
        let request = Request {
            id: 7,
            command: Command::Input {
                input: DeviceInput::Touch {
                    phase: TouchPhase::Down,
                    x: 0.5,
                    y: 0.25,
                },
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"id":7,"cmd":"input","input":{"kind":"touch","phase":"down","x":0.5,"y":0.25}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        let watch: Request = serde_json::from_str(r#"{"id":1,"cmd":"watch","on":true}"#).unwrap();
        assert_eq!(watch.command, Command::Watch { on: true });
        let turn = Request {
            id: 2,
            command: Command::Orientation {
                orientation: DeviceOrientation::LandscapeLeft,
            },
        };
        let json = serde_json::to_string(&turn).unwrap();
        assert_eq!(
            json,
            r#"{"id":2,"cmd":"orientation","orientation":"landscape_left"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), turn);
        let frames: Request =
            serde_json::from_str(r#"{"id":3,"cmd":"accessibility_frames","on":true}"#).unwrap();
        assert_eq!(frames.command, Command::AccessibilityFrames { on: true });
        // A helper that doesn't say which way the screen faces.
        let size: ScreenSize =
            serde_json::from_str(r#"{"width":1,"height":2,"scale":3.0}"#).unwrap();
        assert_eq!(size.orientation, None);
    }

    #[test]
    fn messages_read_back_as_written() {
        let mut out = Vec::new();
        write_event(&mut out, &Event::Done { id: 3, error: None }).unwrap();
        write_message(
            &mut out,
            KIND_FRAME,
            &[&2u32.to_be_bytes(), &1u32.to_be_bytes(), b"jpeg"],
        )
        .unwrap();
        let mut input = &out[..];
        let (kind, body) = read_message(&mut input).unwrap().unwrap();
        assert_eq!(kind, KIND_EVENT);
        assert_eq!(
            serde_json::from_slice::<Event>(&body).unwrap(),
            Event::Done { id: 3, error: None }
        );
        let (kind, body) = read_message(&mut input).unwrap().unwrap();
        assert_eq!(kind, KIND_FRAME);
        assert_eq!(split_frame(&body), Some((2, 1, &b"jpeg"[..])));
        assert!(read_message(&mut input).unwrap().is_none());
    }
}

//! A small gRPC client for the Android emulator's control service
//! (`android.emulation.control.EmulatorController`), over plain HTTP/2 on
//! localhost. gRPC on the wire is a POST per call whose body is a series of
//! messages, each a flag byte, a big-endian `u32` length and the protobuf
//! bytes; the outcome is in the `grpc-status` trailer. The messages below
//! are written by hand from the emulator's own `emulator_controller.proto`
//! (shipped in the SDK's `emulator/lib`), keeping only the fields used.

use anyhow::{Context as _, anyhow, bail};
use bytes::{Buf as _, BufMut as _, Bytes, BytesMut};
use prost::Message;
use std::time::Duration;
use tokio::sync::watch;

const SERVICE: &str = "/android.emulation.control.EmulatorController/";
/// Per-stream receive window: a whole picture in flight at once.
const WINDOW: u32 = 8 << 20;

/// One HTTP/2 connection to an emulator. Cheap to clone; calls run side by
/// side on it.
#[derive(Clone)]
pub struct Grpc {
    send: h2::client::SendRequest<Bytes>,
    port: u16,
    token: Option<String>,
    closed: watch::Receiver<bool>,
}

impl Grpc {
    pub async fn connect(port: u16, token: Option<String>) -> anyhow::Result<Self> {
        let tcp = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .map_err(|_| anyhow!("the emulator's control port {port} didn't answer"))?
        .with_context(|| format!("connecting to the emulator's control port {port}"))?;
        tcp.set_nodelay(true)?;
        let (send, connection) = h2::client::Builder::new()
            .initial_window_size(WINDOW)
            .initial_connection_window_size(WINDOW * 2)
            .max_frame_size(1 << 20)
            .handshake::<_, Bytes>(tcp)
            .await
            .context("the emulator's control service refused HTTP/2")?;
        let (closed_tx, closed) = watch::channel(false);
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(port, "emulator gRPC connection ended: {e}");
            }
            closed_tx.send_replace(true);
        });
        Ok(Self {
            send,
            port,
            token,
            closed,
        })
    }

    /// Whether the connection is gone (the emulator stopped).
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Resolves once the connection is gone.
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|c| *c).await;
    }

    async fn open(
        &self,
        method: &str,
    ) -> anyhow::Result<(h2::client::ResponseFuture, h2::SendStream<Bytes>)> {
        let mut request =
            http::Request::post(format!("http://127.0.0.1:{}{SERVICE}{method}", self.port))
                .header("content-type", "application/grpc")
                .header("te", "trailers");
        if let Some(token) = &self.token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let request = request.body(())?;
        let mut send = self
            .send
            .clone()
            .ready()
            .await
            .context("the emulator closed its control connection")?;
        Ok(send.send_request(request, false)?)
    }

    /// One request, one reply.
    pub async fn unary<Q: Message, R: Message + Default>(
        &self,
        method: &str,
        request: &Q,
    ) -> anyhow::Result<R> {
        let (response, mut body) = self.open(method).await?;
        body.send_data(frame(request), true)?;
        let mut replies = Replies::new(method, response.await?)?;
        replies
            .next()
            .await?
            .ok_or_else(|| anyhow!("the emulator didn't answer {method}"))
    }

    /// One request, a stream of replies.
    pub async fn server_stream<Q: Message>(
        &self,
        method: &str,
        request: &Q,
    ) -> anyhow::Result<Replies> {
        let (response, mut body) = self.open(method).await?;
        body.send_data(frame(request), true)?;
        Replies::new(method, response.await?)
    }

    /// A stream of requests, kept open: messages sent on it arrive in order.
    pub async fn client_stream(&self, method: &str) -> anyhow::Result<Sender> {
        let (response, body) = self.open(method).await?;
        let method = method.to_owned();
        // Its one answer comes when the stream ends; say if it failed.
        tokio::spawn(async move {
            match response.await {
                Ok(response) => {
                    if let Err(e) = Replies::new(&method, response) {
                        tracing::debug!("emulator {method}: {e:#}");
                    }
                }
                Err(e) => tracing::debug!("emulator {method}: {e}"),
            }
        });
        Ok(Sender { body })
    }
}

/// The open request side of [`Grpc::client_stream`].
pub struct Sender {
    body: h2::SendStream<Bytes>,
}

impl Sender {
    pub fn send<Q: Message>(&mut self, message: &Q) -> anyhow::Result<()> {
        self.body.send_data(frame(message), false)?;
        Ok(())
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let _ = self.body.send_data(Bytes::new(), true);
    }
}

/// A gRPC message on the wire: uncompressed, length first.
pub fn frame<Q: Message>(message: &Q) -> Bytes {
    let len = message.encoded_len();
    let mut out = BytesMut::with_capacity(5 + len);
    out.put_u8(0);
    out.put_u32(len as u32);
    message
        .encode(&mut out)
        .expect("a BytesMut grows as needed");
    out.freeze()
}

/// Take one whole message off the front of `buf`, if one is there.
pub fn unframe(buf: &mut BytesMut) -> anyhow::Result<Option<Bytes>> {
    if buf.len() < 5 {
        return Ok(None);
    }
    if buf[0] != 0 {
        bail!("the emulator sent a compressed message");
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        buf.reserve(5 + len - buf.len());
        return Ok(None);
    }
    buf.advance(5);
    Ok(Some(buf.split_to(len).freeze()))
}

/// The replies of one call, as they come.
pub struct Replies {
    method: String,
    body: h2::RecvStream,
    buf: BytesMut,
    done: bool,
}

impl Replies {
    fn new(method: &str, response: http::Response<h2::RecvStream>) -> anyhow::Result<Self> {
        let (head, body) = response.into_parts();
        if head.status != http::StatusCode::OK {
            bail!("the emulator answered {method} with HTTP {}", head.status);
        }
        // A call that fails at once answers with its status in the headers.
        check_status(method, &head.headers)?;
        Ok(Self {
            method: method.to_owned(),
            body,
            buf: BytesMut::new(),
            done: false,
        })
    }

    /// The next reply; `None` once the call ended well.
    pub async fn next<R: Message + Default>(&mut self) -> anyhow::Result<Option<R>> {
        loop {
            if let Some(message) = unframe(&mut self.buf)? {
                return Ok(Some(R::decode(message)?));
            }
            if self.done {
                return Ok(None);
            }
            match self.body.data().await {
                Some(chunk) => {
                    let chunk = chunk?;
                    let _ = self.body.flow_control().release_capacity(chunk.len());
                    self.buf.extend_from_slice(&chunk);
                }
                None => {
                    self.done = true;
                    if let Some(trailers) = self.body.trailers().await? {
                        check_status(&self.method, &trailers)?;
                    }
                }
            }
        }
    }
}

fn check_status(method: &str, headers: &http::HeaderMap) -> anyhow::Result<()> {
    let Some(status) = headers.get("grpc-status") else {
        return Ok(());
    };
    let code = status.to_str().unwrap_or("2").trim();
    if code == "0" {
        return Ok(());
    }
    let message = headers
        .get("grpc-message")
        .and_then(|m| m.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    match code {
        // UNAUTHENTICATED, PERMISSION_DENIED
        "16" | "7" => bail!(
            "the emulator refused {method}: {message} (it was started outside Agentloft, \
             without its access token)"
        ),
        _ => bail!("the emulator failed {method} (gRPC status {code}): {message}"),
    }
}

/// `grpc-message` is percent-encoded.
fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// The messages, numbered as in `emulator_controller.proto`.

#[derive(Clone, PartialEq, Message)]
pub struct Empty {}

/// `ImageFormat.ImgFormat`
pub mod img_format {
    pub const PNG: i32 = 0;
    pub const RGB888: i32 = 2;
}

#[derive(Clone, PartialEq, Message)]
pub struct ImageFormat {
    #[prost(int32, tag = "1")]
    pub format: i32,
    /// Output only: how the device is turned.
    #[prost(message, optional, tag = "2")]
    pub rotation: Option<Rotation>,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(uint32, tag = "5")]
    pub display: u32,
    /// Output only: the folded screen's size before scaling, while a
    /// foldable is folded.
    #[prost(message, optional, tag = "7")]
    pub folded_display: Option<FoldedDisplay>,
}

/// Where a folded screen lies in the unfolded one.
#[derive(Clone, PartialEq, Message)]
pub struct FoldedDisplay {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
    #[prost(uint32, tag = "3")]
    pub x_offset: u32,
    #[prost(uint32, tag = "4")]
    pub y_offset: u32,
}

/// `Rotation.SkinRotation`: 0 portrait, 1 landscape (turned a quarter
/// counter-clockwise), 2 upside down, 3 the other landscape.
#[derive(Clone, PartialEq, Message)]
pub struct Rotation {
    #[prost(int32, tag = "1")]
    pub rotation: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Image {
    #[prost(message, optional, tag = "1")]
    pub format: Option<ImageFormat>,
    #[prost(uint32, tag = "2")]
    pub width: u32,
    #[prost(uint32, tag = "3")]
    pub height: u32,
    #[prost(bytes = "bytes", tag = "4")]
    pub image: Bytes,
    #[prost(uint32, tag = "5")]
    pub seq: u32,
}

impl Image {
    /// Width, height and turn, from the format (the top-level ones are
    /// deprecated) falling back to them.
    pub fn dims(&self) -> (u32, u32, i32) {
        let format = self.format.as_ref();
        let w = format.map_or(0, |f| f.width).max(self.width);
        let h = format.map_or(0, |f| f.height).max(self.height);
        let turn = format
            .and_then(|f| f.rotation.as_ref())
            .map_or(0, |r| r.rotation);
        (w, h, turn)
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Touch {
    #[prost(int32, tag = "1")]
    pub x: i32,
    #[prost(int32, tag = "2")]
    pub y: i32,
    #[prost(int32, tag = "3")]
    pub identifier: i32,
    /// Non-zero while touching; 0 lifts the finger.
    #[prost(int32, tag = "4")]
    pub pressure: i32,
    #[prost(int32, tag = "5")]
    pub touch_major: i32,
    #[prost(int32, tag = "6")]
    pub touch_minor: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TouchEvent {
    #[prost(message, repeated, tag = "1")]
    pub touches: Vec<Touch>,
    #[prost(int32, tag = "2")]
    pub display: i32,
}

/// `KeyboardEvent.KeyEventType`
pub mod key_event {
    pub const DOWN: i32 = 0;
    pub const UP: i32 = 1;
    pub const PRESS: i32 = 2;
}

/// A key by its W3C `KeyboardEvent.key` name (the emulator maps it to
/// Android's), or text to type.
#[derive(Clone, PartialEq, Message)]
pub struct KeyboardEvent {
    /// `KeyCodeType` of `key_code`: 0 USB, 1 evdev, …
    #[prost(int32, tag = "1")]
    pub code_type: i32,
    #[prost(int32, tag = "2")]
    pub event_type: i32,
    #[prost(int32, tag = "3")]
    pub key_code: i32,
    #[prost(string, tag = "4")]
    pub key: String,
    #[prost(string, tag = "5")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct InputEvent {
    #[prost(oneof = "Input", tags = "1, 2")]
    pub input: Option<Input>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum Input {
    #[prost(message, tag = "1")]
    Key(KeyboardEvent),
    #[prost(message, tag = "2")]
    Touch(TouchEvent),
}

#[derive(Clone, PartialEq, Message)]
pub struct EmulatorStatus {
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(uint64, tag = "2")]
    pub uptime: u64,
    #[prost(bool, tag = "3")]
    pub booted: bool,
}

/// `VmRunState.RunState`
pub mod run_state {
    pub const SHUTDOWN: i32 = 5;
}

#[derive(Clone, PartialEq, Message)]
pub struct VmRunState {
    #[prost(int32, tag = "1")]
    pub state: i32,
}

/// `PhysicalModelValue.PhysicalType`
pub mod physical {
    /// Degrees about x, y, z.
    pub const ROTATION: i32 = 1;
    /// A foldable's (first) hinge, in degrees.
    pub const HINGE_ANGLE0: i32 = 10;
    /// A foldable's posture, as [`super::posture`] numbers them.
    pub const POSTURE: i32 = 16;
}

/// `Posture.PostureValue` (also the AVD's `hw.sensor.posture_list`).
pub mod posture {
    pub const CLOSED: i32 = 1;
    pub const HALF_OPENED: i32 = 2;
    pub const OPENED: i32 = 3;
    pub const FLIPPED: i32 = 4;
    pub const TENT: i32 = 5;
}

#[derive(Clone, PartialEq, Message)]
pub struct Posture {
    #[prost(int32, tag = "3")]
    pub value: i32,
}

/// What `streamNotification` says; only a posture change is read.
#[derive(Clone, PartialEq, Message)]
pub struct Notification {
    #[prost(message, optional, tag = "4")]
    pub posture: Option<Posture>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ParameterValue {
    #[prost(float, repeated, tag = "1")]
    pub data: Vec<f32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PhysicalModelValue {
    #[prost(int32, tag = "1")]
    pub target: i32,
    #[prost(int32, tag = "2")]
    pub status: i32,
    #[prost(message, optional, tag = "3")]
    pub value: Option<ParameterValue>,
    /// 0 smooth, 1 at once.
    #[prost(int32, tag = "4")]
    pub interpolation: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct GpsState {
    #[prost(bool, tag = "1")]
    pub passive_update: bool,
    #[prost(double, tag = "2")]
    pub latitude: f64,
    #[prost(double, tag = "3")]
    pub longitude: f64,
    #[prost(double, tag = "4")]
    pub speed: f64,
    #[prost(double, tag = "5")]
    pub bearing: f64,
    #[prost(double, tag = "6")]
    pub altitude: f64,
    #[prost(int32, tag = "7")]
    pub satellites: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClipData {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let touch = TouchEvent {
            touches: vec![Touch {
                x: 540,
                y: 1200,
                identifier: 0,
                pressure: 1,
                touch_major: 0,
                touch_minor: 0,
            }],
            display: 0,
        };
        let one = frame(&touch);
        assert_eq!(one[0], 0);
        assert_eq!(
            u32::from_be_bytes([one[1], one[2], one[3], one[4]]) as usize,
            one.len() - 5
        );
        // Two messages arriving in pieces come out whole, one at a time.
        let mut buf = BytesMut::new();
        let both = [one.clone(), one].concat();
        buf.extend_from_slice(&both[..3]);
        assert!(unframe(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&both[3..]);
        let first = unframe(&mut buf).unwrap().unwrap();
        assert_eq!(TouchEvent::decode(first).unwrap(), touch);
        assert!(unframe(&mut buf).unwrap().is_some());
        assert!(unframe(&mut buf).unwrap().is_none());
    }

    #[test]
    fn oneof_input_encodes_under_its_tag() {
        let key = InputEvent {
            input: Some(Input::Key(KeyboardEvent {
                code_type: 0,
                key_code: 0,
                event_type: key_event::PRESS,
                key: "GoHome".into(),
                text: String::new(),
            })),
        };
        let bytes = key.encode_to_vec();
        // Field 1, length-delimited.
        assert_eq!(bytes[0], (1 << 3) | 2);
        assert_eq!(InputEvent::decode(bytes.as_slice()).unwrap(), key);
    }

    #[test]
    fn grpc_messages_are_percent_decoded() {
        assert_eq!(percent_decode("bad%20token%3A x"), "bad token: x");
        assert_eq!(percent_decode("100%"), "100%");
    }
}

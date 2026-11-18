//! A phone's screen as pictures, with nothing installed on it: the system's
//! own `screenrecord` streams H.264 through `adb exec-out`, decoded here by
//! VideoToolbox (macOS), else by ffmpeg when it is installed; without
//! either, `screencap` pictures are taken one after another (slow: a few a
//! second).
//!
//! `screenrecord` keeps the size it started with: turned sideways, it
//! letterboxes the screen into it. So a new recording is started at the
//! turned size when the phone turns, and before an old Android's 3-minute
//! limit ends one; the old one goes once the new one has shown its first
//! picture, so the picture never stops.

use super::adb::Adb;
use super::h264::{Splitter, Unit};
use super::touch::Rotation;
use super::{PICTURE_MAX, fit, jpeg_size};
use crate::devices::Picture;
use anyhow::{anyhow, bail};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// At most this many pictures a second.
const PICTURE_EVERY: Duration = Duration::from_millis(33);
const JPEG_QUALITY: u8 = 80;
/// The recording's bit rate: plenty for a phone's screen at this size, and
/// light on Wi-Fi.
const BIT_RATE: &str = "8000000";
/// Once the stream has been quiet this long, the picture read last is
/// taken to be complete, at first; each picture found to have come in
/// parts further apart doubles it, up to [`QUIET_MAX`] (an emulator under
/// load sends a large picture's parts tens of milliseconds apart).
const QUIET_START: u32 = 12;
const QUIET_MAX: u32 = 100;
/// A keyframe (the first picture of a recording) waits at least this long.
const QUIET_KEY: Duration = Duration::from_millis(60);

/// The quiet time learned for one phone, in milliseconds.
#[derive(Debug)]
pub struct Quiet(std::sync::atomic::AtomicU32);

impl Default for Quiet {
    fn default() -> Self {
        Self(QUIET_START.into())
    }
}

impl Quiet {
    fn get(&self, key: bool) -> Duration {
        let ms = Duration::from_millis(self.0.load(std::sync::atomic::Ordering::Relaxed).into());
        if key { ms.max(QUIET_KEY) } else { ms }
    }

    /// A picture came in parts: wait longer from now on.
    fn longer(&self) {
        let ms = self.0.load(std::sync::atomic::Ordering::Relaxed);
        self.0.store(
            (ms * 2).min(QUIET_MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}
/// Start the next recording this long before the current one's limit.
const OVERLAP: Duration = Duration::from_secs(8);
/// An old Android's limit (newer ones take 0 for none).
const OLD_LIMIT: u32 = 180;

/// How pictures are made from the phone's video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decode {
    /// macOS's own decoder (and ImageIO for JPEG).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    VideoToolbox,
    /// An ffmpeg on this computer, turning H.264 into JPEG.
    Ffmpeg(PathBuf),
    /// No video: `screencap` again and again.
    Screencap,
}

impl Decode {
    pub fn name(&self) -> &'static str {
        match self {
            Decode::VideoToolbox => "VideoToolbox",
            Decode::Ffmpeg(_) => "ffmpeg",
            Decode::Screencap => "screencap",
        }
    }
}

/// The best way on this computer, and the next ones to fall back to.
pub fn decoders(ffmpeg: Option<PathBuf>, forced: Option<&str>) -> Vec<Decode> {
    let mut all = Vec::new();
    #[cfg(target_os = "macos")]
    all.push(Decode::VideoToolbox);
    if let Some(ffmpeg) = ffmpeg {
        all.push(Decode::Ffmpeg(ffmpeg));
    }
    all.push(Decode::Screencap);
    if let Some(forced) = forced
        && let Some(i) = all
            .iter()
            .position(|d| d.name().eq_ignore_ascii_case(forced))
    {
        all.drain(..i);
    }
    all
}

#[derive(Clone)]
pub struct Config {
    pub adb: Adb,
    pub serial: String,
    /// The display as built, in pixels.
    pub width: u32,
    pub height: u32,
    pub decoders: Vec<Decode>,
    /// Seconds a recording may last; `None`: ask the phone.
    pub limit: Option<u32>,
    pub quiet: Arc<Quiet>,
}

enum Msg {
    Picture(Arc<Picture>),
    /// A picture was decoded before all of it came: what follows may be
    /// wrong until a new recording (which starts with a keyframe) takes
    /// over.
    Renew,
    /// The recording stopped (its reason when it failed).
    Ended(Option<String>),
}

type Tx = mpsc::UnboundedSender<(u64, Msg)>;

/// One recording, stopped when dropped.
struct Recording {
    nth: u64,
    rotation: Rotation,
    started: Instant,
    pictures: bool,
    task: tokio::task::AbortHandle,
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Keep pictures coming while `watching` is on. Returns when `pictures` has
/// no receivers left; says in `ended` when the phone went away.
pub async fn run(
    cfg: Config,
    mut watching: watch::Receiver<bool>,
    mut rotation: watch::Receiver<Rotation>,
    pictures: watch::Sender<Option<Arc<Picture>>>,
    ended: watch::Sender<Option<String>>,
) {
    let limit = match cfg.limit {
        Some(limit) => limit,
        None => {
            // Newer systems record for as long as asked (`0`); their
            // help says so.
            let help = cfg
                .adb
                .script(&cfg.serial, "screenrecord --help 2>&1")
                .await
                .unwrap_or_default();
            if help.contains("Set to 0") {
                0
            } else {
                OLD_LIMIT
            }
        }
    };
    let mut decoders = cfg.decoders.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut nth = 0u64;
    // Recordings that showed nothing since the last picture; at the
    // native size from the second, with the next decoder from the third.
    let mut fruitless = 0u32;
    loop {
        if watching.wait_for(|on| *on).await.is_err() {
            return;
        }
        let start = |nth: &mut u64, rotation: Rotation, fruitless: u32, decode: &Decode| {
            *nth += 1;
            let task = tokio::spawn(record(
                cfg.clone(),
                *nth,
                rotation,
                fruitless == 0,
                limit,
                decode.clone(),
                tx.clone(),
            ));
            Recording {
                nth: *nth,
                rotation,
                started: Instant::now(),
                pictures: false,
                task: task.abort_handle(),
            }
        };
        let mut current = start(&mut nth, *rotation.borrow(), fruitless, &decoders[0]);
        let mut next: Option<Recording> = None;
        loop {
            let renew = (limit > 0 && next.is_none()).then(|| {
                current.started + Duration::from_secs(limit.into()).saturating_sub(OVERLAP)
            });
            tokio::select! {
                msg = rx.recv() => {
                    let Some((at, msg)) = msg else { return };
                    match msg {
                        Msg::Picture(picture) => {
                            if next.as_ref().is_some_and(|n| n.nth == at) {
                                current = next.take().expect("checked");
                            }
                            if current.nth == at {
                                current.pictures = true;
                                fruitless = 0;
                                pictures.send_replace(Some(picture));
                            }
                        }
                        Msg::Renew => {
                            if current.nth == at && next.is_none() {
                                tracing::debug!(serial = cfg.serial, "a phone picture came in two parts: recording afresh");
                                next = Some(start(&mut nth, *rotation.borrow(), fruitless, &decoders[0]));
                            }
                        }
                        Msg::Ended(why) => {
                            if next.as_ref().is_some_and(|n| n.nth == at) {
                                tracing::debug!(serial = cfg.serial, "a phone's next recording stopped: {why:?}");
                                next = None;
                                continue;
                            }
                            if current.nth != at {
                                continue;
                            }
                            if let Some(n) = next.take() {
                                current = n;
                                continue;
                            }
                            if !current.pictures {
                                fruitless += 1;
                                tracing::debug!(serial = cfg.serial, decode = decoders[0].name(), "a phone's recording showed nothing: {why:?}");
                                if fruitless >= 2 && decoders.len() > 1 {
                                    decoders.remove(0);
                                    tracing::info!(serial = cfg.serial, "phone pictures: falling back to {}", decoders[0].name());
                                    fruitless = 1;
                                }
                                if fruitless >= 3 && !connected(&cfg).await {
                                    ended.send_replace(Some("The phone was disconnected.".into()));
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(300 * fruitless.min(10) as u64)).await;
                            }
                            current = start(&mut nth, *rotation.borrow(), fruitless, &decoders[0]);
                        }
                    }
                }
                _ = async { tokio::time::sleep_until(renew.expect("set")).await }, if renew.is_some() => {
                    next = Some(start(&mut nth, *rotation.borrow(), fruitless, &decoders[0]));
                }
                changed = rotation.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let now = *rotation.borrow_and_update();
                    let target = next.as_ref().unwrap_or(&current).rotation;
                    if now.sideways() != target.sideways() && decoders[0] != Decode::Screencap {
                        next = Some(start(&mut nth, now, fruitless, &decoders[0]));
                    }
                }
                off = async { watching.wait_for(|on| !*on).await.is_ok() } => {
                    if !off {
                        return;
                    }
                    break;
                }
            }
        }
    }
}

async fn connected(cfg: &Config) -> bool {
    cfg.adb
        .run(Some(&cfg.serial), &["get-state"], Duration::from_secs(5))
        .await
        .is_ok_and(|s| s.trim() == "device")
}

/// One recording until it stops: pictures, then `Ended`.
async fn record(
    cfg: Config,
    nth: u64,
    rotation: Rotation,
    sized: bool,
    limit: u32,
    decode: Decode,
    tx: Tx,
) {
    let result = match &decode {
        Decode::Screencap => screencaps(&cfg, nth, &tx).await,
        _ => {
            let (w, h) = rotation.size(cfg.width, cfg.height);
            let size = sized.then(|| record_size(w, h));
            video(&cfg, nth, size, limit, &decode, &tx).await
        }
    };
    let _ = tx.send((nth, Msg::Ended(result.err().map(|e| format!("{e:#}")))));
}

/// A size for the encoder: the screen fitted into [`PICTURE_MAX`], in
/// multiples of 16 (what phones' encoders take best).
pub fn record_size(width: u32, height: u32) -> (u32, u32) {
    let (w, h) = fit(width, height, PICTURE_MAX);
    let r = |v: u32| ((v + 8) / 16 * 16).max(16);
    (r(w), r(h))
}

/// `screenrecord`'s H.264, decoded.
async fn video(
    cfg: &Config,
    nth: u64,
    size: Option<(u32, u32)>,
    limit: u32,
    decode: &Decode,
    tx: &Tx,
) -> anyhow::Result<()> {
    let mut cmd = cfg.adb.command(Some(&cfg.serial))?;
    let size = size.map_or(String::new(), |(w, h)| format!("--size {w}x{h} "));
    // Its pid first, to stop it on the phone when we let go: an orphan
    // would only notice at its next picture, which a still screen never
    // makes.
    let script = format!(
        "echo $$; exec screenrecord --output-format=h264 {size}--bit-rate {BIT_RATE} \
         --time-limit {limit} -"
    );
    cmd.args(["exec-out", &script]);
    let mut child = cmd.spawn()?;
    let mut out = child.stdout.take().ok_or_else(|| anyhow!("no output"))?;
    let mut first = Vec::new();
    let mut chunk = [0u8; 64];
    let newline = loop {
        let n = out.read(&mut chunk).await?;
        if n == 0 {
            bail!("screenrecord didn't start");
        }
        first.extend_from_slice(&chunk[..n]);
        if let Some(i) = first.iter().position(|b| *b == b'\n') {
            break i;
        }
        if first.len() > 32 {
            bail!(
                "screenrecord didn't start: {}",
                String::from_utf8_lossy(&first)
            );
        }
    };
    let pid: u32 = std::str::from_utf8(&first[..newline])
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(|| anyhow!("screenrecord didn't say its pid"))?;
    let _stop = StopOnPhone {
        adb: cfg.adb.clone(),
        serial: cfg.serial.clone(),
        pid,
    };
    let rest = first[newline + 1..].to_vec();
    match decode {
        #[cfg(target_os = "macos")]
        Decode::VideoToolbox => videotoolbox(&mut out, rest, &cfg.quiet, nth, tx).await,
        Decode::Ffmpeg(ffmpeg) => {
            through_ffmpeg(ffmpeg, cfg, &mut out, rest, &cfg.quiet, nth, tx).await
        }
        _ => bail!("not a video decoder"),
    }
}

/// Stops a `screenrecord` on the phone when dropped (if it is still the
/// same process).
struct StopOnPhone {
    adb: Adb,
    serial: String,
    pid: u32,
}

impl Drop for StopOnPhone {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (adb, serial, pid) = (self.adb.clone(), self.serial.clone(), self.pid);
        rt.spawn(async move {
            let _ = adb
                .script(
                    &serial,
                    &format!(
                        "grep -q screenrecord /proc/{pid}/cmdline 2>/dev/null && kill -2 {pid}; true"
                    ),
                )
                .await;
        });
    }
}

/// Read the stream through `splitter`: `on_units` gets the units it gives
/// out, as they come and when the stream pauses (for as long as `quiet`
/// says). A picture found to have come in parts asks for a new recording
/// and makes the wait longer.
async fn read_stream(
    out: &mut tokio::process::ChildStdout,
    first: Vec<u8>,
    quiet: &Quiet,
    nth: u64,
    tx: &Tx,
    mut on_units: impl FnMut(&[u8], Vec<Unit>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut splitter = Splitter::default();
    let mut chunk = vec![0u8; 256 * 1024];
    let mut bytes = first;
    let mut pending = false;
    loop {
        if !bytes.is_empty() {
            let units = splitter.push(&bytes);
            if splitter.take_broken() {
                quiet.longer();
                let _ = tx.send((nth, Msg::Renew));
            }
            on_units(&bytes, units)?;
            pending = true;
        }
        let n = if pending {
            let wait = quiet.get(splitter.pending_key());
            match tokio::time::timeout(wait, out.read(&mut chunk)).await {
                Ok(n) => n?,
                Err(_) => {
                    pending = false;
                    bytes.clear();
                    on_units(&[], splitter.flush())?;
                    continue;
                }
            }
        } else {
            out.read(&mut chunk).await?
        };
        if n == 0 {
            on_units(&[], splitter.flush())?;
            return Ok(());
        }
        bytes.clear();
        bytes.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(target_os = "macos")]
async fn videotoolbox(
    out: &mut tokio::process::ChildStdout,
    first: Vec<u8>,
    quiet: &Quiet,
    nth: u64,
    tx: &Tx,
) -> anyhow::Result<()> {
    use super::videotoolbox::{Decoder, Pixels};
    let latest: Arc<std::sync::Mutex<Option<Pixels>>> = Arc::default();
    let ready = Arc::new(tokio::sync::Notify::new());
    let (units_tx, units_rx) = std::sync::mpsc::channel::<Unit>();
    let (failed_tx, mut failed) = watch::channel::<Option<String>>(None);
    // Decode every picture (each needs the one before), on a thread of its
    // own: VideoToolbox decodes synchronously.
    let decoding = {
        let (latest, ready, tx) = (latest.clone(), ready.clone(), tx.clone());
        std::thread::Builder::new()
            .name("phone-h264".into())
            .spawn(move || {
                let mut decoder: Option<Decoder> = None;
                let mut errors = 0;
                while let Ok(unit) = units_rx.recv() {
                    match unit {
                        Unit::Params { sps, pps } => match Decoder::new(&sps, &pps, PICTURE_MAX) {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                failed_tx.send_replace(Some(format!("{e:#}")));
                                return;
                            }
                        },
                        Unit::Frame { nals, .. } => {
                            let Some(decoder) = decoder.as_mut() else {
                                continue;
                            };
                            match decoder.decode(&nals) {
                                Ok(Some(pixels)) => {
                                    errors = 0;
                                    *latest.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(pixels);
                                    ready.notify_one();
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    // Pictures that refer to a broken one
                                    // fail too: start from a keyframe.
                                    errors += 1;
                                    if errors == 3 {
                                        let _ = tx.send((nth, Msg::Renew));
                                    }
                                    if errors >= 120 {
                                        failed_tx.send_replace(Some(format!("{e:#}")));
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            })?
    };
    // Encode the newest decoded picture, at most every PICTURE_EVERY.
    let encoding = {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                ready.notified().await;
                let started = Instant::now();
                let Some(pixels) = latest.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    continue;
                };
                let jpeg = tokio::task::spawn_blocking(move || {
                    pixels.jpeg(f64::from(JPEG_QUALITY) / 100.0)
                })
                .await;
                match jpeg {
                    Ok(Ok((jpeg, width, height))) => {
                        let picture = Picture {
                            jpeg,
                            width,
                            height,
                        };
                        if tx.send((nth, Msg::Picture(Arc::new(picture)))).is_err() {
                            return;
                        }
                    }
                    Ok(Err(e)) => tracing::debug!("encoding a phone picture: {e:#}"),
                    Err(_) => return,
                }
                tokio::time::sleep_until(started + PICTURE_EVERY).await;
            }
        })
    };
    let _encoding = AbortOnDrop(encoding.abort_handle());
    let read = read_stream(out, first, quiet, nth, tx, |_, units| {
        for unit in units {
            units_tx
                .send(unit)
                .map_err(|_| anyhow!("the decoder stopped"))?;
        }
        Ok(())
    });
    let result = tokio::select! {
        read = read => read,
        _ = failed.wait_for(Option::is_some) => Ok(()),
    };
    // The decoding thread ends once the units stop coming (`units_tx`
    // dropped on return).
    drop(decoding);
    if let Some(why) = failed.borrow().clone() {
        bail!("{why}");
    }
    result
}

#[cfg(target_os = "macos")]
struct AbortOnDrop(tokio::task::AbortHandle);

#[cfg(target_os = "macos")]
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// An access unit delimiter: tells ffmpeg's parser the picture before it is
/// complete (else it waits for the next one).
const AUD: &[u8] = &[0, 0, 0, 1, 9, 0xf0];

/// ffmpeg turns the H.264 into JPEGs (MJPEG on its stdout).
async fn through_ffmpeg(
    ffmpeg: &PathBuf,
    cfg: &Config,
    out: &mut tokio::process::ChildStdout,
    first: Vec<u8>,
    quiet: &Quiet,
    nth: u64,
    tx: &Tx,
) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.envs(cfg.adb.env().iter())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            // Not `-fflags nobuffer`: with it ffmpeg 8 opens its encoder
            // before the first picture and refuses its colour range.
            "-flags",
            "low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-f",
            "h264",
            "-i",
            "-",
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            "-pix_fmt",
            "yuvj420p",
            "-q:v",
            "4",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    super::adb::no_window(&mut cmd);
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no input"))?;
    let mut jpegs = child.stdout.take().ok_or_else(|| anyhow!("no output"))?;
    // H.264 in: pass it on as it comes, with a delimiter when it pauses.
    let (bytes_tx, mut bytes_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let feed = async move {
        while let Some(bytes) = bytes_rx.recv().await {
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
        }
        anyhow::Ok(())
    };
    // The bytes as they come; a delimiter once the stream pauses.
    let read = read_stream(out, first, quiet, nth, tx, |bytes, _| {
        let bytes = if bytes.is_empty() { AUD } else { bytes };
        bytes_tx
            .send(bytes.to_vec())
            .map_err(|_| anyhow!("ffmpeg stopped"))
    });
    // JPEGs out: the newest, at most every PICTURE_EVERY.
    let pictures = async {
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 256 * 1024];
        let mut last_sent = Instant::now() - PICTURE_EVERY;
        let mut held: Option<Vec<u8>> = None;
        loop {
            let wait = (last_sent + PICTURE_EVERY).saturating_duration_since(Instant::now());
            let n = if held.is_some() {
                match tokio::time::timeout(wait, jpegs.read(&mut chunk)).await {
                    Ok(n) => n?,
                    Err(_) => {
                        if let Some(jpeg) = held.take() {
                            send_jpeg(jpeg, nth, tx)?;
                            last_sent = Instant::now();
                        }
                        continue;
                    }
                }
            } else {
                jpegs.read(&mut chunk).await?
            };
            if n == 0 {
                return anyhow::Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            while let Some(jpeg) = next_jpeg(&mut buf) {
                held = Some(jpeg);
            }
            if held.is_some() && last_sent.elapsed() >= PICTURE_EVERY {
                send_jpeg(held.take().expect("checked"), nth, tx)?;
                last_sent = Instant::now();
            }
        }
    };
    tokio::select! {
        r = read => r,
        r = feed => r,
        r = pictures => r,
    }
}

fn send_jpeg(jpeg: Vec<u8>, nth: u64, tx: &Tx) -> anyhow::Result<()> {
    let (width, height) = jpeg_size(&jpeg).unwrap_or((0, 0));
    tx.send((
        nth,
        Msg::Picture(Arc::new(Picture {
            jpeg,
            width,
            height,
        })),
    ))
    .map_err(|_| anyhow!("nobody wants pictures"))
}

/// The first whole JPEG in `buf` (SOI to EOI), taken out of it.
pub fn next_jpeg(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let start = buf.windows(2).position(|w| w == [0xff, 0xd8])?;
    // Inside the picture data a 0xFF is always followed by 0x00 or a
    // restart marker, so 0xFF 0xD9 is the end.
    let end = buf[start + 2..]
        .windows(2)
        .position(|w| w == [0xff, 0xd9])?
        + start
        + 4;
    let jpeg = buf[start..end].to_vec();
    buf.drain(..end);
    Some(jpeg)
}

/// `screencap` again and again: raw pixels (quicker than PNG for the
/// phone), scaled and encoded here.
async fn screencaps(cfg: &Config, nth: u64, tx: &Tx) -> anyhow::Result<()> {
    let mut last = Vec::new();
    loop {
        let started = Instant::now();
        let raw = cfg
            .adb
            .bytes(
                Some(&cfg.serial),
                &["exec-out", "screencap"],
                Duration::from_secs(20),
            )
            .await?;
        // The screen as it was: nothing new to show.
        if raw == last {
            tokio::time::sleep_until(started + PICTURE_EVERY).await;
            continue;
        }
        last.clone_from(&raw);
        let picture = tokio::task::spawn_blocking(move || raw_picture(&raw)).await??;
        tx.send((nth, Msg::Picture(Arc::new(picture))))
            .map_err(|_| anyhow!("nobody wants pictures"))?;
        tokio::time::sleep_until(started + PICTURE_EVERY).await;
    }
}

/// `screencap`'s raw output (a header of width, height, format and, on
/// newer systems, a colour space; then the pixels) as a JPEG picture.
pub fn raw_picture(raw: &[u8]) -> anyhow::Result<Picture> {
    let word =
        |i: usize| -> Option<u32> { Some(u32::from_le_bytes(raw.get(i..i + 4)?.try_into().ok()?)) };
    let (w, h, format) = (
        word(0).unwrap_or(0),
        word(4).unwrap_or(0),
        word(8).unwrap_or(0),
    );
    let pixels = (w as usize) * (h as usize) * 4;
    if w == 0 || h == 0 || raw.len() < pixels + 12 {
        bail!("screencap gave {} bytes, not a picture", raw.len());
    }
    let header = raw.len() - pixels;
    if header != 12 && header != 16 {
        bail!("screencap's picture has a header of {header} bytes");
    }
    let data = &raw[header..];
    let mut rgb = Vec::with_capacity(pixels / 4 * 3);
    match format {
        // RGBA_8888, RGBX_8888.
        1 | 2 => {
            for px in data.as_chunks::<4>().0 {
                rgb.extend_from_slice(&px[..3]);
            }
        }
        // BGRA_8888.
        5 => {
            for px in data.as_chunks::<4>().0 {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        other => bail!("screencap's pixel format {other} isn't known here"),
    }
    let image = image::RgbImage::from_raw(w, h, rgb)
        .ok_or_else(|| anyhow!("a picture of the wrong size"))?;
    let (tw, th) = fit(w, h, PICTURE_MAX);
    let image = if (tw, th) != (w, h) {
        image::imageops::resize(&image, tw, th, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut jpeg = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY);
    image.write_with_encoder(encoder)?;
    Ok(Picture {
        jpeg,
        width: tw,
        height: th,
    })
}

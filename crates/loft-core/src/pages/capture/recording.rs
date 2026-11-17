//! `recording_*`: a tab recorded to a video file.
//!
//! A recording is one more watcher of the page's screencast (ADR-0017 §2):
//! it never starts a screencast of its own, so viewers' pictures are the
//! same with or without it, and a page nobody else watches is pictured for
//! it alone. The pictures it receives are placed on a constant frame-rate
//! grid and handed to a writer thread, which puts them, still JPEGs, into a
//! Matroska file as they come (`mkv.rs`). An `.mp4` or `.webm` is made from
//! that file with ffmpeg once the recording stops.

use super::super::automation::Agent;
use super::super::cdp::Event;
use super::super::{Browser, Page, lock};
use super::pixeldiff::image_size;
use super::{PageCapture, file_in, now_iso};
use base64::Engine as _;
use loft_page_script::{BrowserError, EVAL_TIMEOUT, RecordingOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_FRAME_RATE: u32 = 30;
const DEFAULT_LONGEST_MS: u64 = 15 * 60 * 1000;
const MIN_LONGEST_MS: u64 = 1000;
const MAX_LONGEST_MS: u64 = 4 * 60 * 60 * 1000;
/// Pictures waiting for the writer, at most.
const QUEUE: usize = 30;
/// How often a running recording looks at its tab and its clock.
const CHECK_EVERY: Duration = Duration::from_millis(500);
/// JPEG quality of a picture made again at the file's size, or of the
/// still taken when nothing was pictured.
const JPEG_QUALITY: u8 = 85;

/// The kind of file a recording makes, from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Mkv,
    Mp4,
    Webm,
}

impl Container {
    fn of(path: &Path) -> Result<Self, BrowserError> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("mkv") => Ok(Self::Mkv),
            Some("mp4") => Ok(Self::Mp4),
            Some("webm") => Ok(Self::Webm),
            _ => Err(BrowserError::Action(
                "the name must end in .mkv (MJPEG, no ffmpeg needed), .mp4 or .webm (made with ffmpeg)"
                    .into(),
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Mkv => "mkv",
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
        }
    }

    fn mime_type(self) -> &'static str {
        match self {
            Self::Mkv => "video/x-matroska",
            Self::Mp4 => "video/mp4",
            Self::Webm => "video/webm",
        }
    }
}

/// The page's recording, if one runs, or the result of one that ended by
/// itself and hasn't been collected.
#[derive(Default)]
pub(super) struct RecordingState {
    running: Option<Running>,
    ended: Option<Value>,
    /// Counts recordings, so a timer only ends its own.
    started: u64,
}

struct Running {
    id: u64,
    path: PathBuf,
    partial: PathBuf,
    container: Container,
    frame_rate: u32,
    started: String,
    sink: Arc<Mutex<Sink>>,
    writer: std::thread::JoinHandle<Result<Written, String>>,
    timer: tokio::task::JoinHandle<()>,
}

/// What the writer made.
#[derive(Debug)]
struct Written {
    width: u32,
    height: u32,
    frames: u64,
    rescaled: u64,
}

/// A picture on its way to the writer: base64 JPEG, shown from `at_ms`
/// for `duration_ms`.
struct Frame {
    data: String,
    at_ms: u64,
    duration_ms: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    source: u64,
    encoded: u64,
    coalesced: u64,
    dropped: u64,
}

impl Counts {
    fn json(&self, elapsed: Duration) -> Value {
        let seconds = elapsed.as_secs_f64();
        let rate = if seconds > 0.0 {
            (self.source as f64 / seconds * 10.0).round() / 10.0
        } else {
            0.0
        };
        json!({
            "source_frame_count": self.source,
            "encoded_frame_count": self.encoded,
            "coalesced_frame_count": self.coalesced,
            "dropped_frame_count": self.dropped,
            "source_frame_rate": rate,
        })
    }
}

/// The picture held until the next one shows when it ends.
struct Held {
    data: String,
    slot: u64,
}

/// Where the screencast's pictures go while a recording runs: each one is
/// given a slot on the frame-rate grid and held until the next one with
/// another slot arrives.
pub(super) struct Sink {
    started: Instant,
    frame_rate: u32,
    held: Option<Held>,
    last_slot: Option<u64>,
    queue: Option<SyncSender<Frame>>,
    counts: Counts,
}

impl Sink {
    fn new(frame_rate: u32, queue: SyncSender<Frame>) -> Self {
        Self {
            started: Instant::now(),
            frame_rate,
            held: None,
            last_slot: None,
            queue: Some(queue),
            counts: Counts::default(),
        }
    }

    /// When a slot begins, in milliseconds.
    fn ms(&self, slot: u64) -> u64 {
        slot * 1000 / u64::from(self.frame_rate)
    }

    /// A picture, `at` after the start.
    fn picture(&mut self, data: String, at: Duration) {
        self.counts.source += 1;
        let fps = f64::from(self.frame_rate);
        let seconds = at.as_secs_f64();
        let slot = match self.last_slot {
            None => 0,
            Some(previous) => {
                let slot = ((seconds * fps).floor() as u64).max(previous);
                // Late in the same slot: it starts the next one instead.
                if slot == previous && seconds >= (previous as f64 + 0.5) / fps {
                    previous + 1
                } else {
                    slot
                }
            }
        };
        self.last_slot = Some(slot);
        match self.held.take() {
            Some(held) if held.slot == slot => self.counts.coalesced += 1,
            Some(held) => {
                let frame = self.frame(held.data, held.slot, slot);
                self.hand(frame);
            }
            None => {}
        }
        self.held = Some(Held { data, slot });
    }

    fn frame(&self, data: String, slot: u64, until: u64) -> Frame {
        let at_ms = self.ms(slot);
        Frame {
            data,
            at_ms,
            duration_ms: self.ms(until).saturating_sub(at_ms).max(1),
        }
    }

    /// To the writer, unless its queue is full (or it has gone).
    fn hand(&mut self, frame: Frame) {
        match self.queue.as_ref().map(|q| q.try_send(frame)) {
            Some(Ok(())) => self.counts.encoded += 1,
            Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_))) | None => {
                self.counts.dropped += 1
            }
        }
    }

    /// The last picture (the held one, or `still` at slot 0 when none
    /// came), lasting until the recording's end, `elapsed` after its start.
    fn last(&mut self, still: Option<String>, elapsed: Duration) -> Option<Frame> {
        let (data, slot) = match self.held.take() {
            Some(held) => (held.data, held.slot),
            None => (still?, 0),
        };
        let end = ((elapsed.as_secs_f64() * f64::from(self.frame_rate)).round() as u64)
            .max(1)
            .max(slot + 1);
        Some(self.frame(data, slot, end))
    }
}

/// A screencast picture of the page, for its recording if one runs.
pub(super) fn observe(state: &PageCapture, event: &Event) {
    let Some(sink) = lock(&state.frames).clone() else {
        return;
    };
    let Some(data) = event.params["data"].as_str() else {
        return;
    };
    let mut sink = lock(&sink);
    let at = sink.started.elapsed();
    sink.picture(data.to_owned(), at);
}

/// ffmpeg, if this computer has it: in the daemon's `PATH`, or where
/// installers put it (a daemon launchd started has a short `PATH`).
pub(super) fn find_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(loft_config::PATH_SEPARATOR)
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .chain(
            [
                "/opt/homebrew/bin/ffmpeg",
                "/usr/local/bin/ffmpeg",
                "/usr/bin/ffmpeg",
            ]
            .map(PathBuf::from),
        )
        .find(|candidate| candidate.is_file())
}

/// `recording_start`.
pub(super) async fn start(
    agent: &Agent<'_>,
    state: &Arc<PageCapture>,
    options: RecordingOptions,
) -> Result<Value, BrowserError> {
    let frame_rate = options.frame_rate.unwrap_or(DEFAULT_FRAME_RATE);
    if !(1..=60).contains(&frame_rate) {
        return Err(BrowserError::Action(
            "frame_rate must be from 1 to 60".into(),
        ));
    }
    let longest = options.max_duration_ms.unwrap_or(DEFAULT_LONGEST_MS);
    if !(MIN_LONGEST_MS..=MAX_LONGEST_MS).contains(&longest) {
        return Err(BrowserError::Action(format!(
            "max_duration_ms must be from {MIN_LONGEST_MS} to {MAX_LONGEST_MS}"
        )));
    }
    let mut recording = state.recording.lock().await;
    if recording.running.is_some() {
        return Err(BrowserError::Action(
            "a recording is already running in this tab; stop or cancel it first".into(),
        ));
    }
    let path = file_in(agent.page, options.name.as_deref(), "recording", "mkv")?;
    let container = Container::of(&path)?;
    if container != Container::Mkv && find_ffmpeg().is_none() {
        return Err(BrowserError::Action(format!(
            "ffmpeg is needed for .{} and isn't installed here; record to .mkv instead",
            container.name()
        )));
    }
    let partial = beside(&path, "mkv");
    let file = create_private(&partial)
        .map_err(|e| BrowserError::Action(format!("couldn't start {}: {e}", partial.display())))?;
    let (queue, frames) = std::sync::mpsc::sync_channel(QUEUE);
    let writer = std::thread::Builder::new()
        .name("recording writer".into())
        .spawn(move || write(file, frames))
        .map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            BrowserError::Action(format!("couldn't start the recording's writer: {e}"))
        })?;
    let sink = Arc::new(Mutex::new(Sink::new(frame_rate, queue)));
    *lock(&state.frames) = Some(sink.clone());
    agent.browser.watching(agent.page, true).await;
    recording.started += 1;
    let id = recording.started;
    let timer = tokio::spawn(ends_by_itself(
        agent.browser.clone(),
        agent.page.clone(),
        state.clone(),
        id,
        Duration::from_millis(longest),
    ));
    recording.ended = None;
    recording.running = Some(Running {
        id,
        path: path.clone(),
        partial,
        container,
        frame_rate,
        started: now_iso(),
        sink,
        writer,
        timer,
    });
    Ok(json!({
        "recording": true,
        "path": path,
        "format": container.name(),
        "frame_rate": frame_rate,
        "note": "the file is finished when the recording stops",
    }))
}

/// `recording_status`.
pub(super) async fn status(state: &PageCapture) -> Value {
    let recording = state.recording.lock().await;
    let Some(running) = &recording.running else {
        return recording
            .ended
            .clone()
            .unwrap_or_else(|| json!({ "recording": false }));
    };
    let (counts, elapsed) = {
        let sink = lock(&running.sink);
        (sink.counts, sink.started.elapsed())
    };
    json!({
        "recording": true,
        "path": running.path,
        "format": running.container.name(),
        "started": running.started,
        "duration_ms": elapsed.as_millis() as u64,
        "frame_rate": running.frame_rate,
        "quality": counts.json(elapsed),
    })
}

/// `recording_stop`.
pub(super) async fn stop(agent: &Agent<'_>, state: &PageCapture) -> Result<Value, BrowserError> {
    let mut recording = state.recording.lock().await;
    let Some(running) = recording.running.take() else {
        return recording
            .ended
            .take()
            .ok_or_else(|| BrowserError::Action("no recording is running in this tab".into()));
    };
    running.timer.abort();
    finish(agent.browser, agent.page, state, running).await
}

/// `recording_cancel`.
pub(super) async fn cancel(agent: &Agent<'_>, state: &PageCapture) -> Value {
    let mut recording = state.recording.lock().await;
    recording.ended = None;
    let Some(running) = recording.running.take() else {
        return json!({ "cancelled": false });
    };
    running.timer.abort();
    stop_watching(agent.browser, agent.page, state, &running.sink).await;
    // Its queue goes, so the writer ends.
    lock(&running.sink).queue = None;
    let writer = running.writer;
    let _ = tokio::task::spawn_blocking(move || writer.join()).await;
    let _ = std::fs::remove_file(&running.partial);
    json!({ "cancelled": true })
}

/// Ends the recording when it has run its longest, or when its tab
/// closes, keeping what it made for the next status or stop.
async fn ends_by_itself(
    browser: Arc<Browser>,
    page: Arc<Page>,
    state: Arc<PageCapture>,
    id: u64,
    longest: Duration,
) {
    let began = Instant::now();
    let because = loop {
        let left = longest.saturating_sub(began.elapsed());
        tokio::time::sleep(left.min(CHECK_EVERY)).await;
        if !open(&browser, &page) {
            break "its tab closed".to_owned();
        }
        if began.elapsed() >= longest {
            break format!(
                "it reached its longest ({} s, max_duration_ms)",
                longest.as_secs()
            );
        }
    };
    let mut recording = state.recording.lock().await;
    if recording.running.as_ref().is_none_or(|r| r.id != id) {
        return;
    }
    let Some(running) = recording.running.take() else {
        return;
    };
    let mut result = match finish(&browser, &page, &state, running).await {
        Ok(result) => result,
        Err(err) => json!({ "error": err.to_string(), "recording": false }),
    };
    result["ended_because"] = json!(because);
    recording.ended = Some(result);
}

fn open(browser: &Browser, page: &Page) -> bool {
    !browser.dead.load(Ordering::Relaxed) && lock(&browser.pages).contains_key(&page.id)
}

/// No more pictures for this recording, and one watcher fewer.
async fn stop_watching(
    browser: &Arc<Browser>,
    page: &Arc<Page>,
    state: &PageCapture,
    sink: &Arc<Mutex<Sink>>,
) {
    {
        let mut frames = lock(&state.frames);
        if frames.as_ref().is_some_and(|s| Arc::ptr_eq(s, sink)) {
            *frames = None;
        }
    }
    if open(browser, page) {
        browser.watching(page, false).await;
    }
}

/// Finish the file: the last picture, the writer closed, the file in its
/// place (converted with ffmpeg for `.mp4` and `.webm`).
async fn finish(
    browser: &Arc<Browser>,
    page: &Arc<Page>,
    state: &PageCapture,
    running: Running,
) -> Result<Value, BrowserError> {
    stop_watching(browser, page, state, &running.sink).await;
    let (elapsed, held) = {
        let sink = lock(&running.sink);
        (sink.started.elapsed(), sink.held.is_some())
    };
    // Nothing was pictured (nothing moved, or the page never painted): one
    // still of the page.
    let still = if held {
        None
    } else {
        still(browser, page).await
    };
    let (last, queue, counts) = {
        let mut sink = lock(&running.sink);
        let last = sink.last(still, elapsed);
        if last.is_some() {
            sink.counts.encoded += 1;
        }
        (last, sink.queue.take(), sink.counts)
    };
    let writer = running.writer;
    let written = tokio::task::spawn_blocking(move || {
        // Never dropped: it waits for room.
        if let (Some(queue), Some(last)) = (queue, last) {
            let _ = queue.send(last);
        }
        writer.join()
    })
    .await;
    let written = match written {
        Ok(Ok(Ok(written))) => written,
        Ok(Ok(Err(err))) => {
            let _ = std::fs::remove_file(&running.partial);
            return Err(BrowserError::Action(format!("the recording failed: {err}")));
        }
        Ok(Err(_)) | Err(_) => {
            let _ = std::fs::remove_file(&running.partial);
            return Err(BrowserError::Action(
                "the recording's writer stopped".into(),
            ));
        }
    };
    let path = &running.path;
    match running.container {
        Container::Mkv => {
            if let Err(err) = std::fs::rename(&running.partial, path) {
                let _ = std::fs::remove_file(&running.partial);
                return Err(BrowserError::Action(format!(
                    "couldn't finish {}: {err}",
                    path.display()
                )));
            }
        }
        container => {
            let made = convert(
                &running.partial,
                path,
                container,
                running.frame_rate,
                (written.width, written.height),
            )
            .await;
            let _ = std::fs::remove_file(&running.partial);
            made.map_err(BrowserError::Action)?;
        }
    }
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut quality = counts.json(elapsed);
    if written.rescaled > 0 {
        quality["rescaled_frame_count"] = json!(written.rescaled);
    }
    Ok(json!({
        "recording": false,
        "path": path,
        "format": running.container.name(),
        "mime_type": running.container.mime_type(),
        "bytes": bytes,
        "duration_ms": elapsed.as_millis() as u64,
        "width": written.width,
        "height": written.height,
        "frame_rate": running.frame_rate,
        "frame_count": written.frames,
        "quality": quality,
    }))
}

/// One JPEG of the page as it is (base64).
async fn still(browser: &Browser, page: &Page) -> Option<String> {
    let _shaping = page.shaping.lock().await;
    let shot = browser
        .cdp
        .send_within(
            "Page.captureScreenshot",
            json!({ "format": "jpeg", "quality": JPEG_QUALITY, "optimizeForSpeed": true }),
            Some(&page.session),
            EVAL_TIMEOUT,
        )
        .await
        .ok()?;
    shot["data"].as_str().map(str::to_owned)
}

/// A temporary file beside `path`: `.<name>.partial.<ext>`.
fn beside(path: &Path, ext: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.partial.{ext}"))
}

fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// The writer thread: frames into the file until the queue closes. The
/// file's size is its first frame's; other sizes are scaled to it.
fn write(file: std::fs::File, frames: Receiver<Frame>) -> Result<Written, String> {
    let mut file = Some(file);
    let mut writer: Option<super::mkv::Writer<std::io::BufWriter<std::fs::File>>> = None;
    let (mut width, mut height, mut rescaled) = (0, 0, 0);
    let failed = |e: std::io::Error| format!("couldn't write the file: {e}");
    for frame in frames {
        let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(&frame.data) else {
            continue;
        };
        let size = image_size(&jpeg);
        if let Some(file) = file.take() {
            (width, height) = size.unwrap_or((2, 2));
            writer = Some(
                super::mkv::Writer::new(std::io::BufWriter::new(file), width, height)
                    .map_err(failed)?,
            );
        }
        let Some(out) = writer.as_mut() else {
            break;
        };
        let jpeg = if size == Some((width, height)) {
            jpeg
        } else {
            let Some(again) = rescale(&jpeg, width, height) else {
                continue;
            };
            rescaled += 1;
            again
        };
        out.frame(&jpeg, frame.at_ms, frame.duration_ms)
            .map_err(failed)?;
    }
    let Some(writer) = writer else {
        return Err("no video frames were captured".into());
    };
    let frames = writer.frames();
    let file = writer
        .finish()
        .map_err(failed)?
        .into_inner()
        .map_err(|e| failed(e.into_error()))?;
    file.sync_all().map_err(failed)?;
    Ok(Written {
        width,
        height,
        frames,
        rescaled,
    })
}

/// A picture made again at the file's size.
fn rescale(jpeg: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let picture = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .to_rgb8();
    let resized = image::imageops::resize(
        &picture,
        width,
        height,
        image::imageops::FilterType::Triangle,
    );
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&resized)
        .ok()?;
    Some(out)
}

/// Make `output` (`.mp4` or `.webm`) from the recording's Matroska file.
async fn convert(
    input: &Path,
    output: &Path,
    container: Container,
    frame_rate: u32,
    (width, height): (u32, u32),
) -> Result<(), String> {
    let ffmpeg = find_ffmpeg().ok_or_else(|| {
        "couldn't run ffmpeg (it isn't installed here any more); record to .mkv instead".to_owned()
    })?;
    let temporary = beside(output, container.name());
    let (width, height) = ((width / 2 * 2).max(2), (height / 2 * 2).max(2));
    // MJPEG's colour is full range; video players expect limited.
    let filter = format!(
        "fps={frame_rate},scale={width}:{height}:flags=lanczos:in_range=pc:out_range=tv,format=yuv420p"
    );
    let mut command = tokio::process::Command::new(ffmpeg);
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args(["-an", "-vf", &filter, "-color_range", "tv"]);
    match container {
        Container::Mp4 => command.args([
            "-c:v",
            "libx264",
            "-preset",
            "fast",
            "-crf",
            "14",
            "-tune",
            "animation",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ]),
        _ => command.args([
            "-c:v",
            "libvpx",
            "-crf",
            "4",
            "-b:v",
            "6M",
            "-deadline",
            "good",
            "-cpu-used",
            "4",
            "-threads",
            "2",
            "-f",
            "webm",
        ]),
    };
    command
        .arg(&temporary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let ran = command.output().await;
    let failure = match ran {
        Err(err) => format!("couldn't run ffmpeg ({err}); record to .mkv instead"),
        Ok(out) if out.status.success() => match std::fs::rename(&temporary, output) {
            Ok(()) => return Ok(()),
            Err(err) => format!("couldn't finish {}: {err}", output.display()),
        },
        Ok(out) => {
            let said = String::from_utf8_lossy(&out.stderr);
            let chars: Vec<char> = said.chars().collect();
            let tail: String = chars[chars.len().saturating_sub(2000)..].iter().collect();
            format!("ffmpeg failed ({}): {}", out.status, tail.trim())
        }
    };
    let _ = std::fs::remove_file(&temporary);
    Err(failure)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(width: u32, height: u32) -> String {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&image::RgbImage::from_pixel(
                width,
                height,
                image::Rgb([30, 140, 220]),
            ))
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    #[test]
    fn the_container_is_the_names() {
        assert_eq!(Container::of(Path::new("a.MKV")).unwrap(), Container::Mkv);
        assert_eq!(Container::of(Path::new("a.mp4")).unwrap(), Container::Mp4);
        assert_eq!(Container::of(Path::new("a.webm")).unwrap(), Container::Webm);
        let gif = Container::of(Path::new("a.gif")).unwrap_err();
        assert!(gif.to_string().contains("must end in .mkv"), "{gif}");
        assert_eq!(
            beside(Path::new("/x/demo.mp4"), "mkv"),
            Path::new("/x/.demo.mp4.partial.mkv")
        );
    }

    #[test]
    fn pictures_go_on_a_constant_grid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grid.mkv");
        let (queue, frames) = std::sync::mpsc::sync_channel(QUEUE);
        let file = create_private(&path).unwrap();
        let writer = std::thread::spawn(move || write(file, frames));
        let mut sink = Sink::new(10, queue);
        let ms = Duration::from_millis;
        sink.picture(jpeg(64, 48), ms(0));
        sink.picture(jpeg(64, 48), ms(120));
        sink.picture(jpeg(64, 48), ms(130));
        sink.picture(jpeg(32, 24), ms(500));
        let last = sink.last(None, ms(800)).unwrap();
        assert_eq!((last.at_ms, last.duration_ms), (500, 300));
        sink.queue.take().unwrap().send(last).unwrap();
        assert_eq!(
            sink.counts,
            Counts {
                source: 4,
                encoded: 2,
                coalesced: 1,
                dropped: 0
            }
        );
        let written = writer.join().unwrap().unwrap();
        assert_eq!(
            (
                written.width,
                written.height,
                written.frames,
                written.rescaled
            ),
            (64, 48, 3, 1)
        );
        let Ok(out) = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries"])
            .args(["stream=codec_name,width,height,nb_read_frames:format=duration"])
            .args(["-of", "compact"])
            .arg(&path)
            .output()
        else {
            eprintln!("no ffprobe here: skipped");
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("codec_name=mjpeg|width=64|height=48|nb_read_frames=3"),
            "{text}"
        );
        assert!(text.contains("duration=0.8"), "{text}");
    }

    #[test]
    fn late_pictures_take_the_next_slot_and_full_queues_drop() {
        let (queue, _frames) = std::sync::mpsc::sync_channel(1);
        let mut sink = Sink::new(10, queue);
        let ms = Duration::from_millis;
        sink.picture("a".into(), ms(0));
        // Half a slot into slot 0: the next one.
        sink.picture("b".into(), ms(60));
        assert_eq!(sink.last_slot, Some(1));
        sink.picture("c".into(), ms(300));
        assert_eq!(sink.counts.encoded, 1);
        assert_eq!(sink.counts.dropped, 1, "the queue holds one");
        // Nothing came at all: the still lasts a slot at least.
        let (queue, _frames) = std::sync::mpsc::sync_channel(1);
        let mut empty = Sink::new(30, queue);
        assert!(empty.last(None, ms(5)).is_none());
        let still = empty.last(Some("still".into()), ms(5)).unwrap();
        assert_eq!((still.at_ms, still.duration_ms), (0, 33));
    }
}

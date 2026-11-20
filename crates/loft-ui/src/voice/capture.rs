//! Recording the microphone for dictation: whatever the device gives,
//! collected as 16 kHz mono 16-bit samples, the form the daemon's speech
//! engine takes.
//!
//! - Native (macOS, Windows, Linux): `cpal` on a thread of its own
//!   (`capture/native.rs`). `AGENTLOFT_FAKE_MIC=<file.wav>` plays a WAV file
//!   in real time instead of opening a microphone.
//! - Browser: `getUserMedia` and Web Audio (`capture/web.rs`).
//!
//! Both feed a [`Collector`]: down to one channel, resampled to
//! [`SAMPLE_RATE`], kept up to [`MAX_SECONDS`], with a running loudness for
//! the level meter.

use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(not(target_arch = "wasm32"))]
mod native;
mod resample;
#[cfg(not(target_arch = "wasm32"))]
mod wav;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
use native as platform;
#[cfg(target_arch = "wasm32")]
use web as platform;

use resample::Resampler;

/// The rate recordings come out at.
pub const SAMPLE_RATE: u32 = 16_000;
/// A recording stops collecting after this many seconds.
pub const MAX_SECONDS: u32 = 300;

/// A microphone being recorded. Dropping it without [`Recorder::finish`]
/// cancels: the microphone is released at once.
pub struct Recorder {
    collector: Arc<Mutex<Collector>>,
    /// Releases the microphone when dropped.
    _source: platform::Source,
}

impl Recorder {
    /// Opens the default microphone; asks the OS or browser for permission
    /// when it must. Errors are a sentence a user can act on.
    pub async fn start() -> anyhow::Result<Recorder> {
        let (collector, source) = platform::start().await?;
        Ok(Recorder {
            collector,
            _source: source,
        })
    }

    /// Loudness of roughly the last 50 ms, 0.0..=1.0, for a level meter.
    pub fn level(&self) -> f32 {
        self.lock().level()
    }

    /// Seconds recorded so far.
    pub fn seconds(&self) -> f32 {
        self.lock().seconds()
    }

    /// Why recording stopped by itself, if it did.
    pub fn ended(&self) -> Option<String> {
        self.lock().ended.clone()
    }

    /// The samples recorded since the first `from` (for live
    /// transcription, which sends them as they come).
    pub fn samples_from(&self, from: usize) -> Vec<i16> {
        let collector = self.lock();
        collector.samples.get(from..).unwrap_or_default().to_vec()
    }

    /// Stops and returns everything recorded: 16 kHz mono.
    pub fn finish(self) -> Vec<i16> {
        self.lock().finish()
    }

    fn lock(&self) -> MutexGuard<'_, Collector> {
        lock(&self.collector)
    }
}

fn lock(collector: &Mutex<Collector>) -> MutexGuard<'_, Collector> {
    collector
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where a recording's samples go, from whichever thread delivers them.
pub(crate) struct Collector {
    channels: usize,
    resampler: Resampler,
    samples: Vec<i16>,
    /// Squares of the last [`LEVEL_WINDOW`] samples (a ring) and their sum.
    window: Vec<f32>,
    cursor: usize,
    window_sum: f64,
    ended: Option<String>,
    finished: bool,
    mono: Vec<f32>,
    converted: Vec<f32>,
}

/// Samples at [`SAMPLE_RATE`] a recording keeps.
const MAX_SAMPLES: usize = (SAMPLE_RATE * MAX_SECONDS) as usize;
/// The level meter shows the last 50 ms.
const LEVEL_WINDOW: usize = SAMPLE_RATE as usize / 20;
/// Loudness (RMS, dB below full scale) shown as an empty and a full meter.
const QUIET_DB: f32 = -60.;
const LOUD_DB: f32 = -12.;

impl Collector {
    /// For `channels` interleaved channels at `rate`.
    pub(crate) fn new(rate: u32, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            resampler: Resampler::new(rate, SAMPLE_RATE),
            samples: Vec::new(),
            window: vec![0.; LEVEL_WINDOW],
            cursor: 0,
            window_sum: 0.,
            ended: None,
            finished: false,
            mono: Vec::new(),
            converted: Vec::new(),
        }
    }

    /// Takes interleaved frames, converting each sample with `to_f32`
    /// (full scale is -1.0..=1.0).
    pub(crate) fn push<T: Copy>(&mut self, data: &[T], to_f32: impl Fn(T) -> f32) {
        if self.finished || self.ended.is_some() {
            return;
        }
        let channels = self.channels;
        self.mono.clear();
        self.mono.extend(
            data.chunks_exact(channels)
                .map(|frame| frame.iter().map(|&s| to_f32(s)).sum::<f32>() / channels as f32),
        );
        let mut converted = std::mem::take(&mut self.converted);
        converted.clear();
        self.resampler.process(&self.mono, &mut converted);
        self.keep(&converted);
        self.converted = converted;
    }

    fn keep(&mut self, converted: &[f32]) {
        for &x in converted {
            if self.samples.len() >= MAX_SAMPLES {
                self.quiet();
                self.ended = Some(format!(
                    "Recording stopped at {} minutes, the longest a dictation can be.",
                    MAX_SECONDS / 60
                ));
                return;
            }
            let square = x * x;
            self.window_sum += f64::from(square) - f64::from(self.window[self.cursor]);
            self.window[self.cursor] = square;
            self.cursor = (self.cursor + 1) % LEVEL_WINDOW;
            self.samples.push(to_i16(x));
        }
    }

    fn quiet(&mut self) {
        self.window.fill(0.);
        self.window_sum = 0.;
    }

    /// Recording stopped by itself: `why`, unless it already had.
    pub(crate) fn end(&mut self, why: impl Into<String>) {
        if self.ended.is_none() && !self.finished {
            self.quiet();
            self.ended = Some(why.into());
        }
    }

    pub(crate) fn level(&self) -> f32 {
        // A running sum drifts by rounding: what is left of silence is ~0.
        level((self.window_sum / LEVEL_WINDOW as f64).max(0.) as f32)
    }

    pub(crate) fn seconds(&self) -> f32 {
        self.samples.len() as f32 / SAMPLE_RATE as f32
    }

    /// Everything recorded; anything delivered later is ignored.
    pub(crate) fn finish(&mut self) -> Vec<i16> {
        if !self.finished && self.ended.is_none() {
            let mut tail = Vec::new();
            self.resampler.flush(&mut tail);
            self.keep(&tail);
        }
        self.finished = true;
        std::mem::take(&mut self.samples)
    }
}

fn to_i16(x: f32) -> i16 {
    (x * f32::from(i16::MAX))
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// A meter position for a mean square: decibels, mapped linearly from
/// [`QUIET_DB`] (0) to [`LOUD_DB`] (1), which is how loudness is heard.
fn level(mean_square: f32) -> f32 {
    if mean_square <= 0. {
        return 0.;
    }
    let db = 10. * mean_square.log10();
    ((db - QUIET_DB) / (LOUD_DB - QUIET_DB)).clamp(0., 1.)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, seconds: f32, amplitude: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| amplitude * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn stereo_48k_becomes_16k_mono_i16() {
        let mut c = Collector::new(48_000, 2);
        let left = sine(48_000, 1_000., 1., 0.5);
        // The right channel is silent: the mix is half as loud.
        let stereo: Vec<f32> = left.iter().flat_map(|&l| [l, 0.]).collect();
        for chunk in stereo.chunks(960) {
            c.push(chunk, |x| x);
        }
        assert!((c.seconds() - 1.).abs() < 0.01);
        let samples = c.finish();
        assert_eq!(samples.len(), 16_000);
        let peak = samples[4_000..12_000]
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap();
        assert!((i32::from(peak) - 8_192).abs() < 100, "{peak}");
        // Nothing comes in after finishing.
        c.push(&[0.5f32, 0.5], |x| x);
        assert!(c.finish().is_empty());
    }

    #[test]
    fn integer_samples_convert() {
        let mut c = Collector::new(SAMPLE_RATE, 1);
        c.push(&[i16::MAX, 0, i16::MIN + 1], |s| f32::from(s) / 32_767.);
        c.push(&[u16::MAX, 32_768], |s| (f32::from(s) - 32_768.) / 32_767.);
        assert_eq!(c.finish(), vec![i16::MAX, 0, -i16::MAX, i16::MAX, 0]);
    }

    #[test]
    fn level_follows_loudness() {
        let mut c = Collector::new(SAMPLE_RATE, 1);
        assert_eq!(c.level(), 0.);
        c.push(&vec![0f32; 1_600], |x| x);
        assert_eq!(c.level(), 0.);
        // A full-scale tone fills the meter.
        c.push(&sine(SAMPLE_RATE, 440., 0.2, 1.), |x| x);
        assert!(c.level() > 0.99, "{}", c.level());
        // 36 dB below full-scale RMS sits in the middle.
        let quiet = 10f32.powf(-36. / 20.) * std::f32::consts::SQRT_2;
        c.push(&sine(SAMPLE_RATE, 440., 0.5, quiet), |x| x);
        assert!((c.level() - 0.5).abs() < 0.03, "{}", c.level());
        // Half of the window silent: 3 dB down…
        c.push(&vec![0f32; 400], |x| x);
        assert!((c.level() - 0.5 + 3. / 48.).abs() < 0.03, "{}", c.level());
        // …and 50 ms of silence empties it.
        c.push(&vec![0f32; 400], |x| x);
        assert_eq!(c.level(), 0.);
    }

    #[test]
    fn stops_collecting_at_the_limit() {
        let mut c = Collector::new(SAMPLE_RATE, 1);
        let second = vec![0.1f32; SAMPLE_RATE as usize];
        for _ in 0..MAX_SECONDS {
            c.push(&second, |x| x);
        }
        assert!(c.ended.is_none());
        assert_eq!(c.seconds(), MAX_SECONDS as f32);
        c.push(&[0.1f32], |x| x);
        assert!(c.ended.as_deref().unwrap().contains("5 minutes"));
        assert_eq!(c.level(), 0.);
        c.push(&second, |x| x);
        assert_eq!(c.finish().len(), MAX_SAMPLES);
    }

    #[test]
    fn ending_keeps_what_was_recorded() {
        let mut c = Collector::new(SAMPLE_RATE, 1);
        c.push(&[0.5f32; 100], |x| x);
        c.end("The microphone was disconnected.");
        c.end("something later");
        c.push(&[0.5f32; 100], |x| x);
        assert_eq!(c.ended.as_deref(), Some("The microphone was disconnected."));
        assert_eq!(c.finish().len(), 100);
    }
}

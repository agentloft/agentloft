//! The microphone on macOS, Windows and Linux, through `cpal`.
//!
//! The stream lives on a thread of its own (a `cpal::Stream` can't move
//! between threads everywhere), which opens it, says whether that worked,
//! then waits until the [`Source`] is dropped and closes it. Its callback
//! feeds the shared [`Collector`].

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use futures::channel::oneshot;

use super::{Collector, lock, wav};

/// Holds the recording thread open; dropping it stops the recording and
/// releases the microphone.
pub(super) struct Source {
    _stop: mpsc::Sender<()>,
}

/// Plays this WAV file instead of opening a microphone.
const FAKE_MIC: &str = "AGENTLOFT_FAKE_MIC";

const NO_MICROPHONE: &str = "No microphone found. Connect one (or pick an input in the system's sound settings) and try again.";

pub(super) async fn start() -> Result<(Arc<Mutex<Collector>>, Source)> {
    if let Some(path) = std::env::var_os(FAKE_MIC) {
        return start_fake(Path::new(&path)).await;
    }
    let (ready, opened) = oneshot::channel();
    let (stop, stopped) = mpsc::channel();
    std::thread::Builder::new()
        .name("microphone".into())
        .spawn(move || {
            let (stream, collector) = match open() {
                Ok(opened) => opened,
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            };
            // If nobody waits any more, the microphone is released at once.
            if ready.send(Ok(collector)).is_ok() {
                let _ = stopped.recv();
            }
            drop(stream);
        })
        .context("couldn't start the microphone thread")?;
    let collector = opened
        .await
        .map_err(|_| anyhow!("The microphone stopped before it started."))??;
    Ok((collector, Source { _stop: stop }))
}

fn open() -> Result<(cpal::Stream, Arc<Mutex<Collector>>)> {
    #[cfg(target_os = "macos")]
    macos::ensure_permission()?;
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| anyhow!(NO_MICROPHONE))?;
    let config = device.default_input_config().map_err(explain)?;
    let format = config.sample_format();
    if !supported(format) {
        return Err(anyhow!(
            "This microphone delivers {format} samples, which Agentloft can't record yet. Pick another input in the system's sound settings."
        ));
    }
    let collector = Arc::new(Mutex::new(Collector::new(
        config.sample_rate(),
        usize::from(config.channels()),
    )));
    let on_data = collector.clone();
    let on_error = collector.clone();
    let stream = device
        .build_input_stream_raw(
            config.config(),
            format,
            move |data, _: &cpal::InputCallbackInfo| push(&mut lock(&on_data), data),
            move |error: cpal::Error| stream_error(&mut lock(&on_error), &error),
            None,
        )
        .map_err(explain)?;
    stream.play().map_err(explain)?;
    Ok((stream, collector))
}

fn supported(format: cpal::SampleFormat) -> bool {
    use cpal::SampleFormat as F;
    matches!(
        format,
        F::F32 | F::F64 | F::I8 | F::I16 | F::I32 | F::U8 | F::U16 | F::U32
    )
}

fn push(collector: &mut Collector, data: &cpal::Data) {
    use cpal::SampleFormat as F;
    fn take<T: cpal::SizedSample>(c: &mut Collector, data: &cpal::Data)
    where
        f32: cpal::FromSample<T>,
    {
        if let Some(samples) = data.as_slice::<T>() {
            c.push(samples, |s| s.to_sample::<f32>());
        }
    }
    match data.sample_format() {
        F::F32 => take::<f32>(collector, data),
        F::F64 => take::<f64>(collector, data),
        F::I8 => take::<i8>(collector, data),
        F::I16 => take::<i16>(collector, data),
        F::I32 => take::<i32>(collector, data),
        F::U8 => take::<u8>(collector, data),
        F::U16 => take::<u16>(collector, data),
        F::U32 => take::<u32>(collector, data),
        _ => {}
    }
}

fn stream_error(collector: &mut Collector, error: &cpal::Error) {
    use cpal::ErrorKind as K;
    match error.kind() {
        // Glitches, and a stream that followed the default input elsewhere.
        K::Xrun | K::DeviceChanged | K::RealtimeDenied => {}
        K::DeviceNotAvailable | K::StreamInvalidated => {
            collector.end("The microphone was disconnected.");
        }
        _ => collector.end(format!("The microphone stopped: {error}")),
    }
}

/// A sentence for a failure to open the microphone.
fn explain(error: cpal::Error) -> anyhow::Error {
    use cpal::ErrorKind as K;
    let text = error.to_string();
    // Windows' privacy setting refuses with E_ACCESSDENIED, which cpal
    // doesn't sort into a kind of its own.
    let refused = error.kind() == K::PermissionDenied
        || (cfg!(windows) && (text.contains("0x80070005") || text.contains("Access is denied")));
    if refused {
        return anyhow!(if cfg!(windows) {
            "Windows doesn't let Agentloft use the microphone. Turn on \"Let desktop apps access your microphone\" in Settings ▸ Privacy & security ▸ Microphone, then try again."
        } else if cfg!(target_os = "macos") {
            "Agentloft isn't allowed to use the microphone. Allow it in System Settings ▸ Privacy & Security ▸ Microphone, then try again."
        } else {
            "The microphone can't be opened: this account isn't allowed to use the sound device (on most systems, membership of the \"audio\" group)."
        });
    }
    match error.kind() {
        K::DeviceNotAvailable => anyhow!(NO_MICROPHONE),
        K::DeviceBusy => anyhow!(
            "The microphone is in use by another app. Close that app or pick another input, then try again."
        ),
        K::HostUnavailable => {
            anyhow!("The system's sound service isn't running, so the microphone can't be opened.")
        }
        _ => anyhow!("The microphone couldn't be opened: {text}"),
    }
}

/// [`FAKE_MIC`]: the file is delivered as a microphone would, in real
/// time, then silence until the recording is finished.
async fn start_fake(path: &Path) -> Result<(Arc<Mutex<Collector>>, Source)> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("{FAKE_MIC}: couldn't read {}", path.display()))?;
    let wav = wav::parse(&bytes).with_context(|| format!("{FAKE_MIC}: {}", path.display()))?;
    let collector = Arc::new(Mutex::new(Collector::new(wav.rate, wav.channels)));
    let (stop, stopped) = mpsc::channel();
    let feed = collector.clone();
    std::thread::Builder::new()
        .name("fake microphone".into())
        .spawn(move || play(&wav, &feed, &stopped))
        .context("couldn't start the fake microphone")?;
    Ok((collector, Source { _stop: stop }))
}

fn play(wav: &wav::Wav, collector: &Mutex<Collector>, stopped: &Receiver<()>) {
    const CHUNK: Duration = Duration::from_millis(20);
    let frames = (wav.rate as usize / 50).max(1);
    let silence = vec![0i16; frames * wav.channels];
    let mut chunks = wav.samples.chunks(frames * wav.channels);
    let started = Instant::now();
    for tick in 1u32.. {
        let chunk = chunks.next().unwrap_or(&silence);
        lock(collector).push(chunk, |s| f32::from(s) / 32_768.);
        let due = started + CHUNK * tick;
        match stopped.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Err(RecvTimeoutError::Timeout) => {}
            // The recorder was dropped (or finished).
            _ => return,
        }
    }
}

/// macOS records silence from a microphone the app may not use, without
/// an error, so the permission is checked (and asked for) first.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // objc2 calls into AVFoundation; each block has a SAFETY note.
mod macos {
    use anyhow::{Context as _, Result, bail};
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    const DENIED: &str = "Agentloft isn't allowed to use the microphone. Allow it in System Settings ▸ Privacy & Security ▸ Microphone, then try again.";

    pub(super) fn ensure_permission() -> Result<()> {
        // SAFETY: a constant NSString AVFoundation exports.
        let audio = unsafe { AVMediaTypeAudio }.context("AVFoundation has no audio media type")?;
        // SAFETY: a class method that reads this app's privacy state; the
        // media type is the audio one, as it requires.
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
        match status {
            AVAuthorizationStatus::Authorized => Ok(()),
            AVAuthorizationStatus::Denied => bail!(DENIED),
            AVAuthorizationStatus::Restricted => bail!(
                "This Mac's settings don't allow apps to use the microphone (a device-management profile or Screen Time). See System Settings ▸ Privacy & Security ▸ Microphone."
            ),
            // Not asked yet: macOS asks the user now. This runs on the
            // microphone thread, which waits for the answer.
            _ => {
                let (tx, rx) = std::sync::mpsc::channel();
                let answered = block2::RcBlock::new(move |granted: Bool| {
                    let _ = tx.send(granted.as_bool());
                });
                // SAFETY: as above; the block is copied by AVFoundation and
                // called once, on a queue of its own, with the answer.
                unsafe {
                    AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &answered);
                }
                if rx.recv() == Ok(true) {
                    Ok(())
                } else {
                    bail!(DENIED)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_file(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = b"RIFF".to_vec();
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * u32::from(channels) * 2).to_le_bytes());
        out.extend((channels * 2).to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    #[test]
    fn the_fake_microphone_plays_a_file_in_real_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        // 0.4 s of a loud 440 Hz tone, stereo at 48 kHz (wide enough that a
        // busy CI runner oversleeping 120 ms still lands inside the tone).
        let tone: Vec<i16> = (0..19_200)
            .flat_map(|i| {
                let s =
                    (16_000. * (std::f32::consts::TAU * 440. * i as f32 / 48_000.).sin()) as i16;
                [s, s]
            })
            .collect();
        std::fs::write(&path, wav_file(48_000, 2, &tone)).unwrap();

        let (collector, source) = futures::executor::block_on(start_fake(&path)).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        {
            let c = lock(&collector);
            let seconds = c.seconds();
            assert!((0.06..0.38).contains(&seconds), "{seconds}");
            assert!(c.level() > 0.8, "{}", c.level());
        }
        // After the file: silence, still counting.
        std::thread::sleep(Duration::from_millis(500));
        {
            let c = lock(&collector);
            assert!(c.seconds() > 0.55, "{}", c.seconds());
            assert!(c.level() < 0.1, "{}", c.level());
        }
        let samples = lock(&collector).finish();
        assert!(samples.len() > 6_400);
        let peak = samples[400..2_800]
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap();
        assert!((15_500..16_500).contains(&peak), "{peak}");
        drop(source);
        // Dropping the source stops the feed at once.
        let after = lock(&collector).seconds();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(lock(&collector).seconds(), after);
    }

    #[test]
    fn the_fake_microphone_explains_a_bad_file() {
        let missing = futures::executor::block_on(start_fake(Path::new("/nonexistent/x.wav")));
        assert!(missing.err().unwrap().to_string().contains(FAKE_MIC));
    }

    /// With `AGENTLOFT_FAKE_MIC=<file.wav>` set, records through the public
    /// API for two seconds. Without it, does nothing (it never opens a real
    /// microphone).
    #[test]
    #[ignore]
    fn records_the_fake_microphone_from_the_environment() {
        let Some(path) = std::env::var_os(FAKE_MIC) else {
            eprintln!("{FAKE_MIC} isn't set: skipped");
            return;
        };
        let recorder = futures::executor::block_on(super::super::Recorder::start()).unwrap();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            let bar = "#".repeat((recorder.level() * 40.) as usize);
            eprintln!("{:5.2}s {bar}", recorder.seconds());
        }
        assert!(recorder.ended().is_none());
        let samples = recorder.finish();
        eprintln!(
            "{} samples from {}",
            samples.len(),
            Path::new(&path).display()
        );
        assert!(samples.len() >= 31_000, "{}", samples.len());
        if let Some(out) = std::env::var_os("AGENTLOFT_FAKE_MIC_OUT") {
            let bytes = wav_file(super::super::SAMPLE_RATE, 1, &samples);
            std::fs::write(out, bytes).unwrap();
        }
    }
}

//! The microphone in the browser: `getUserMedia`, played into an
//! `AudioContext` (16 kHz if the browser agrees) whose script processor
//! hands each block of samples to the [`Collector`].

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioContextOptions, AudioProcessingEvent, DomException, MediaDevices,
    MediaStream, MediaStreamAudioSourceNode, MediaStreamConstraints, MediaStreamTrack,
    ScriptProcessorNode,
};

use super::{Collector, SAMPLE_RATE, lock};

/// Samples per block the processor hands over: 64 ms at 16 kHz.
const BLOCK: u32 = 1_024;

/// The open microphone; dropping it stops the tracks (the browser's
/// recording indicator goes away) and closes the audio graph.
pub(super) struct Source {
    stream: MediaStream,
    graph: Option<Graph>,
    on_ended: Closure<dyn FnMut()>,
}

struct Graph {
    context: AudioContext,
    input: MediaStreamAudioSourceNode,
    processor: ScriptProcessorNode,
    _on_audio: Closure<dyn FnMut(AudioProcessingEvent)>,
}

impl Drop for Source {
    fn drop(&mut self) {
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
                track.set_onended(None);
                track.stop();
            }
        }
        if let Some(graph) = self.graph.take() {
            graph.close();
        }
    }
}

impl Graph {
    fn close(self) {
        self.processor.set_onaudioprocess(None);
        let _ = self.input.disconnect();
        let _ = self.processor.disconnect();
        let _ = self.context.close();
    }
}

pub(super) async fn start() -> Result<(Arc<Mutex<Collector>>, Source)> {
    let window = web_sys::window().ok_or_else(|| anyhow!("No browser window."))?;
    let navigator = window.navigator();
    // `mediaDevices` exists only on secure pages (HTTPS, or localhost).
    let devices = js_sys::Reflect::get(&navigator, &JsValue::from_str("mediaDevices"))
        .ok()
        .filter(|d| !d.is_undefined() && !d.is_null());
    let Some(devices) = devices.filter(|_| window.is_secure_context()) else {
        return Err(anyhow!(
            "The browser allows the microphone only on secure pages. Open Agentloft over HTTPS (or on localhost) to dictate."
        ));
    };
    let devices: MediaDevices = devices.unchecked_into();
    let constraints = MediaStreamConstraints::new();
    constraints.set_audio_bool(true);
    let asked = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(refused)?;
    let stream: MediaStream = JsFuture::from(asked)
        .await
        .map_err(refused)?
        .unchecked_into();

    let mut source = Source {
        stream,
        graph: None,
        on_ended: Closure::new(|| {}),
    };
    let collector = Arc::new(Mutex::new(Collector::new(SAMPLE_RATE, 1)));
    // Ended by the browser: unplugged, or the permission was taken back.
    let ended = collector.clone();
    source.on_ended = Closure::new(move || lock(&ended).end("The microphone was disconnected."));
    for track in source.stream.get_audio_tracks().iter() {
        if let Ok(track) = track.dyn_into::<MediaStreamTrack>() {
            track.set_onended(Some(source.on_ended.as_ref().unchecked_ref()));
        }
    }

    // 16 kHz saves resampling, but some browsers can't join a microphone
    // to a context at another rate than its own: then the default rate.
    let graph = match graph(&source.stream, Some(SAMPLE_RATE), &collector) {
        Ok(graph) => graph,
        Err(_) => graph(&source.stream, None, &collector).map_err(|e| {
            anyhow!(
                "The browser couldn't record the microphone: {}",
                describe(&e)
            )
        })?,
    };
    // A context made outside a click may start suspended.
    if let Ok(resumed) = graph.context.resume() {
        let _ = JsFuture::from(resumed).await;
    }
    source.graph = Some(graph);
    Ok((collector, source))
}

/// The audio graph: microphone → processor (one channel) → output. The
/// processor writes nothing, so the output stays silent, but browsers run
/// a processor only while it is connected to one.
fn graph(
    stream: &MediaStream,
    rate: Option<u32>,
    collector: &Arc<Mutex<Collector>>,
) -> Result<Graph, JsValue> {
    let options = AudioContextOptions::new();
    if let Some(rate) = rate {
        options.set_sample_rate(rate as f32);
    }
    let context = AudioContext::new_with_context_options(&options)?;
    let built = (|| {
        let input = context.create_media_stream_source(stream)?;
        let processor = context
            .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
                BLOCK, 1, 1,
            )?;
        let actual = context.sample_rate().round() as u32;
        *lock(collector) = Collector::new(actual, 1);
        let feed = collector.clone();
        let on_audio =
            Closure::<dyn FnMut(AudioProcessingEvent)>::new(move |event: AudioProcessingEvent| {
                let Ok(buffer) = event.input_buffer() else {
                    return;
                };
                if let Ok(samples) = buffer.get_channel_data(0) {
                    lock(&feed).push(&samples, |s| s);
                }
            });
        processor.set_onaudioprocess(Some(on_audio.as_ref().unchecked_ref()));
        input.connect_with_audio_node(&processor)?;
        processor.connect_with_audio_node(&context.destination())?;
        Ok(Graph {
            context: context.clone(),
            input,
            processor,
            _on_audio: on_audio,
        })
    })();
    if built.is_err() {
        let _ = context.close();
    }
    built
}

/// A sentence for `getUserMedia`'s refusal.
fn refused(error: JsValue) -> anyhow::Error {
    let name = error
        .dyn_ref::<DomException>()
        .map(|e| e.name())
        .unwrap_or_default();
    anyhow!(match name.as_str() {
        "NotAllowedError" | "SecurityError" => {
            "The browser blocked the microphone for this page. Allow it in the site's settings (the icon at the left of the address bar), then try again."
                .to_owned()
        }
        "NotFoundError" | "OverconstrainedError" => {
            "No microphone found. Connect one and try again.".to_owned()
        }
        "NotReadableError" | "AbortError" => {
            "The microphone couldn't be opened: another app may be using it, or the system doesn't let this browser use it (on a Mac: System Settings ▸ Privacy & Security ▸ Microphone)."
                .to_owned()
        }
        _ => format!("The microphone couldn't be opened: {}", describe(&error)),
    })
}

fn describe(error: &JsValue) -> String {
    if let Some(e) = error.dyn_ref::<DomException>() {
        format!("{} ({})", e.message(), e.name())
    } else if let Some(e) = error.dyn_ref::<js_sys::Error>() {
        String::from(e.message())
    } else {
        format!("{error:?}")
    }
}

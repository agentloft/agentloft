//! Voice input requests (`Request::Voice*`): the speech models and their
//! downloads, transcription, and transcript cleanup. The models run in
//! `loft_voice`'s engine processes; cleanup by an agent goes through
//! [`Core::ask_once`] with the instructions `loft_voice::cleanup` writes.

use crate::{Core, CoreResult};
use loft_protocol::{
    ErrorBody, ErrorCode, HarnessId, Response, VOICE_MAX_SECONDS, VOICE_SAMPLE_RATE,
};

/// Where `[voice] cleanup` sends a transcript.
#[derive(Debug, PartialEq)]
pub(crate) enum Cleanup {
    Off,
    /// A model of `loft_voice`'s on this computer.
    Local(String),
    /// An agent's CLI, on the user's subscription; `None`: its default model.
    Agent(HarnessId, Option<String>),
}

impl Cleanup {
    pub(crate) fn parse(setting: &str) -> Self {
        let setting = setting.trim();
        if setting.is_empty() || setting == "off" {
            return Self::Off;
        }
        let (kind, model) = match setting.split_once(':') {
            Some((kind, model)) => (kind, Some(model.trim()).filter(|m| !m.is_empty())),
            None => (setting, None),
        };
        match (kind, model) {
            ("local", Some(model)) => Self::Local(model.to_owned()),
            ("local", None) => Self::Off,
            (agent, model) => Self::Agent(HarnessId(agent.to_owned()), model.map(str::to_owned)),
        }
    }
}

impl Core {
    pub(crate) fn voice_status(&self) -> CoreResult<Response> {
        Ok(Response::Voice {
            status: self.voice.status(),
        })
    }

    pub(crate) fn voice_download(&self, model: &str) -> CoreResult<Response> {
        self.voice.download(model).map_err(bad_request)?;
        self.voice_status()
    }

    pub(crate) fn voice_cancel_download(&self, model: &str) -> CoreResult<Response> {
        self.voice.cancel_download(model);
        self.voice_status()
    }

    pub(crate) async fn voice_remove(&self, model: &str) -> CoreResult<Response> {
        self.voice.remove(model).await.map_err(internal)?;
        self.voice_status()
    }

    pub(crate) async fn voice_unload(&self) -> CoreResult<Response> {
        self.voice.unload().await;
        self.voice_status()
    }

    pub(crate) async fn voice_transcribe(
        &self,
        audio: Vec<u8>,
        language: Option<String>,
        model: Option<String>,
    ) -> CoreResult<Response> {
        let max = (VOICE_SAMPLE_RATE * VOICE_MAX_SECONDS * 2) as usize;
        if audio.len() > max {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!(
                    "Recordings can be up to {} minutes long",
                    VOICE_MAX_SECONDS / 60
                ),
            ));
        }
        let voice = self.config().voice;
        let model = model
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(voice.model.clone());
        if model.trim().is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "Choose a speech model first: Settings ▸ Voice",
            ));
        }
        let language = language.unwrap_or(voice.language.clone());
        let words = voice
            .words
            .iter()
            .map(|w| w.trim().to_owned())
            .filter(|w| !w.is_empty())
            .take(100)
            .collect();
        let text = self
            .voice
            .transcribe(&model, audio, Some(language), words, voice.unload_after())
            .await
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("{e:#}")))?;
        Ok(Response::Text { text })
    }

    pub(crate) async fn voice_cleanup(
        &self,
        transcript: String,
        before: String,
        after: String,
    ) -> CoreResult<Response> {
        if transcript.trim().is_empty() {
            return Ok(Response::Text { text: transcript });
        }
        let voice = self.config().voice;
        let system = loft_voice::cleanup::system(&voice.cleanup_instructions);
        let user = loft_voice::cleanup::user(&transcript, &before, &after);
        let answer = match Cleanup::parse(&voice.cleanup) {
            Cleanup::Off => return Ok(Response::Text { text: transcript }),
            Cleanup::Local(model) => self
                .voice
                .generate(
                    &model,
                    loft_voice::cleanup::local_system(&voice.cleanup_instructions),
                    loft_voice::cleanup::local_user(&transcript),
                    voice.unload_after(),
                )
                .await
                .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("{e:#}")))?,
            Cleanup::Agent(harness, model) => {
                let model = model.or_else(|| cheap_model(&harness));
                match self
                    .ask_once(format!("{system}\n\n{user}"), harness, model)
                    .await?
                {
                    Response::Text { text } => text,
                    _ => String::new(),
                }
            }
        };
        let text = loft_voice::cleanup::tidy(&answer, &transcript)
            .map(|t| loft_voice::cleanup::without_context(t, &before, &after))
            .unwrap_or(transcript);
        Ok(Response::Text { text })
    }
}

/// The agent's small model when none is named: Claude's Haiku; the others'
/// catalogs change too often to name one, so their default.
fn cheap_model(harness: &HarnessId) -> Option<String> {
    (harness.as_str() == HarnessId::CLAUDE).then(|| "haiku".to_owned())
}

fn bad_request(e: anyhow::Error) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, format!("{e:#}"))
}

fn internal(e: anyhow::Error) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_settings_parse() {
        assert_eq!(Cleanup::parse(""), Cleanup::Off);
        assert_eq!(Cleanup::parse("off"), Cleanup::Off);
        assert_eq!(Cleanup::parse("local:"), Cleanup::Off);
        assert_eq!(
            Cleanup::parse("local:qwen3-1.7b"),
            Cleanup::Local("qwen3-1.7b".into())
        );
        assert_eq!(
            Cleanup::parse("claude:haiku"),
            Cleanup::Agent(HarnessId::claude(), Some("haiku".into()))
        );
        assert_eq!(
            Cleanup::parse("codex"),
            Cleanup::Agent(HarnessId("codex".into()), None)
        );
        assert_eq!(
            Cleanup::parse("opencode:openai/gpt-5-mini"),
            Cleanup::Agent(
                HarnessId("opencode".into()),
                Some("openai/gpt-5-mini".into())
            )
        );
    }
}

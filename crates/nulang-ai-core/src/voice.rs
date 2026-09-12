//! Provider-neutral voice primitives for the NuLang agent runtime.
//!
//! Voice is treated as an input/output transport that terminates at NuLang's
//! intent/capability layer rather than at a specific speech or model vendor.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};
use uuid::Uuid;

pub type VoiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioFrame {
    pub sequence: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub pcm_s16le: Vec<i16>,
}

impl AudioFrame {
    pub fn mono_16khz(sequence: u64, pcm_s16le: Vec<i16>) -> Self {
        Self {
            sequence,
            sample_rate_hz: 16_000,
            channels: 1,
            pcm_s16le,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VoiceActivity {
    Silence,
    Speech,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    Partial,
    Final,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub kind: TranscriptKind,
    pub confidence: Option<f32>,
    pub language: Option<String>,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoiceSession {
    pub id: Uuid,
    pub conversation_id: Option<Uuid>,
    pub tenant_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl VoiceSession {
    pub fn new(conversation_id: Option<Uuid>) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id,
            tenant_id: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceEvent {
    SessionStarted {
        session_id: Uuid,
    },
    AudioStarted {
        session_id: Uuid,
    },
    SpeechStarted {
        session_id: Uuid,
    },
    TranscriptUpdated {
        session_id: Uuid,
        transcript: Transcript,
    },
    TurnCompleted {
        session_id: Uuid,
        transcript: Transcript,
    },
    ResponseStarted {
        session_id: Uuid,
    },
    ResponseInterrupted {
        session_id: Uuid,
    },
    SessionEnded {
        session_id: Uuid,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecognitionConfig {
    pub language: Option<String>,
    pub enable_partials: bool,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

impl Default for RecognitionConfig {
    fn default() -> Self {
        Self {
            language: None,
            enable_partials: true,
            sample_rate_hz: 16_000,
            channels: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpeechRequest {
    pub text: String,
    pub voice: Option<String>,
    pub language: Option<String>,
    pub speed: Option<f32>,
}

impl SpeechRequest {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice: None,
            language: None,
            speed: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VoiceProviderError {
    pub provider: String,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl VoiceProviderError {
    pub fn new(
        provider: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            provider: provider.into(),
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

pub trait VoiceActivityDetector: Send + Sync {
    fn provider_name(&self) -> &str;

    fn detect<'a>(
        &'a self,
        frame: &'a AudioFrame,
    ) -> VoiceFuture<'a, Result<VoiceActivity, VoiceProviderError>>;
}

pub trait SpeechRecognitionSession: Send {
    fn push_audio<'a>(
        &'a mut self,
        frame: AudioFrame,
    ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>>;

    fn finish<'a>(&'a mut self) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>>;
}

pub trait SpeechRecognizer: Send + Sync {
    fn provider_name(&self) -> &str;

    fn start_session<'a>(
        &'a self,
        config: RecognitionConfig,
    ) -> VoiceFuture<'a, Result<Box<dyn SpeechRecognitionSession>, VoiceProviderError>>;
}

pub trait SpeechSynthesisStream: Send {
    fn next_frame<'a>(
        &'a mut self,
    ) -> VoiceFuture<'a, Result<Option<AudioFrame>, VoiceProviderError>>;

    fn cancel<'a>(&'a mut self) -> VoiceFuture<'a, Result<(), VoiceProviderError>>;
}

pub trait SpeechSynthesizer: Send + Sync {
    fn provider_name(&self) -> &str;

    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
    ) -> VoiceFuture<'a, Result<Box<dyn SpeechSynthesisStream>, VoiceProviderError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_event_roundtrip_json() {
        let session = VoiceSession::new(None);
        let event = VoiceEvent::TranscriptUpdated {
            session_id: session.id,
            transcript: Transcript {
                text: "review the auth code".into(),
                kind: TranscriptKind::Partial,
                confidence: Some(0.93),
                language: Some("en".into()),
                started_at_ms: Some(0),
                ended_at_ms: Some(840),
            },
        };

        let json = serde_json::to_string(&event).unwrap();
        let decoded: VoiceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, decoded);
        assert!(json.contains("transcript_updated"));
    }

    #[test]
    fn default_recognition_config_is_realtime_friendly() {
        let config = RecognitionConfig::default();
        assert!(config.enable_partials);
        assert_eq!(config.sample_rate_hz, 16_000);
        assert_eq!(config.channels, 1);
    }
}

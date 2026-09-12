use nulang_ai_core::voice::{AudioFrame, VoiceEvent, VoiceSessionId};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveKitSessionConfig {
    pub room: String,
    pub participant_identity: String,
    pub publish_audio: bool,
    pub subscribe_audio: bool,
}

impl LiveKitSessionConfig {
    pub fn duplex(room: impl Into<String>, participant_identity: impl Into<String>) -> Self {
        Self { room: room.into(), participant_identity: participant_identity.into(), publish_audio: true, subscribe_audio: true }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveKitEvent {
    Connected { room: String },
    Disconnected,
    RemoteAudio(AudioFrame),
    Data(Vec<u8>),
}

/// Media-plane adapter seam. A platform-specific LiveKit implementation only
/// needs to translate SDK callbacks into this event enum.
pub struct LiveKitSessionBridge {
    session_id: VoiceSessionId,
    inbound_tx: mpsc::Sender<VoiceEvent>,
}

impl LiveKitSessionBridge {
    pub fn new(session_id: VoiceSessionId, inbound_tx: mpsc::Sender<VoiceEvent>) -> Self {
        Self { session_id, inbound_tx }
    }

    pub fn session_id(&self) -> VoiceSessionId { self.session_id }

    pub async fn handle_event(&self, event: LiveKitEvent) -> Result<(), mpsc::error::SendError<VoiceEvent>> {
        match event {
            LiveKitEvent::RemoteAudio(frame) => self.inbound_tx.send(VoiceEvent::AudioFrame { frame }).await?,
            LiveKitEvent::Disconnected => self.inbound_tx.send(VoiceEvent::SessionEnded).await?,
            LiveKitEvent::Connected { .. } | LiveKitEvent::Data(_) => {}
        }
        Ok(())
    }
}

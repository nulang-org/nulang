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
        Self {
            room: room.into(),
            participant_identity: participant_identity.into(),
            publish_audio: true,
            subscribe_audio: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveKitEvent {
    Connected { room: String },
    Disconnected,
    RemoteAudio(AudioFrame),
    Data(Vec<u8>),
}

/// Runtime-neutral bridge between a LiveKit/WebRTC adapter and NuLang's voice
/// event stream.
///
/// The actual LiveKit SDK lives in a thin process/platform adapter. This type
/// defines the durable boundary expected by the NuLang runtime and can be used
/// unchanged by Rust-native, browser, mobile, or sidecar integrations.
pub struct LiveKitSessionBridge {
    session_id: VoiceSessionId,
    inbound_tx: mpsc::Sender<VoiceEvent>,
}

impl LiveKitSessionBridge {
    pub fn new(session_id: VoiceSessionId, inbound_tx: mpsc::Sender<VoiceEvent>) -> Self {
        Self {
            session_id,
            inbound_tx,
        }
    }

    pub fn session_id(&self) -> VoiceSessionId {
        self.session_id
    }

    pub async fn handle_event(&self, event: LiveKitEvent) -> Result<(), mpsc::error::SendError<VoiceEvent>> {
        match event {
            LiveKitEvent::RemoteAudio(frame) => {
                self.inbound_tx.send(VoiceEvent::AudioFrame { frame }).await?;
            }
            LiveKitEvent::Disconnected => {
                self.inbound_tx.send(VoiceEvent::SessionEnded).await?;
            }
            LiveKitEvent::Connected { .. } | LiveKitEvent::Data(_) => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::voice::{AudioEncoding, AudioFrame};

    #[tokio::test]
    async fn forwards_remote_audio_into_voice_event_stream() {
        let (tx, mut rx) = mpsc::channel(4);
        let session_id = VoiceSessionId::new();
        let bridge = LiveKitSessionBridge::new(session_id, tx);
        let frame = AudioFrame {
            sequence: 1,
            sample_rate_hz: 16_000,
            channels: 1,
            encoding: AudioEncoding::PcmS16Le,
            data: vec![1, 2, 3, 4],
        };

        bridge
            .handle_event(LiveKitEvent::RemoteAudio(frame.clone()))
            .await
            .unwrap();

        assert_eq!(rx.recv().await, Some(VoiceEvent::AudioFrame { frame }));
    }
}

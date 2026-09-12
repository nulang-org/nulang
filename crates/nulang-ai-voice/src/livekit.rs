use nulang_ai_core::voice::{AudioFrame, VoiceEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

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

/// Provider-neutral boundary for a platform-specific LiveKit SDK integration.
///
/// Raw media is forwarded on `audio_tx`; lifecycle signals are translated to
/// NuLang `VoiceEvent`s. This keeps media transport separate from semantic
/// transcript/turn events and avoids coupling `nulang-ai-core` to LiveKit.
pub struct LiveKitSessionBridge {
    session_id: Uuid,
    audio_tx: mpsc::Sender<AudioFrame>,
    event_tx: mpsc::Sender<VoiceEvent>,
}

impl LiveKitSessionBridge {
    pub fn new(
        session_id: Uuid,
        audio_tx: mpsc::Sender<AudioFrame>,
        event_tx: mpsc::Sender<VoiceEvent>,
    ) -> Self {
        Self {
            session_id,
            audio_tx,
            event_tx,
        }
    }

    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    pub async fn handle_event(&self, event: LiveKitEvent) -> Result<(), LiveKitBridgeError> {
        match event {
            LiveKitEvent::Connected { .. } => {
                self.event_tx
                    .send(VoiceEvent::AudioStarted {
                        session_id: self.session_id,
                    })
                    .await
                    .map_err(|_| LiveKitBridgeError::EventChannelClosed)?;
            }
            LiveKitEvent::RemoteAudio(frame) => {
                self.audio_tx
                    .send(frame)
                    .await
                    .map_err(|_| LiveKitBridgeError::AudioChannelClosed)?;
            }
            LiveKitEvent::Disconnected => {
                self.event_tx
                    .send(VoiceEvent::SessionEnded {
                        session_id: self.session_id,
                    })
                    .await
                    .map_err(|_| LiveKitBridgeError::EventChannelClosed)?;
            }
            LiveKitEvent::Data(_) => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveKitBridgeError {
    AudioChannelClosed,
    EventChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwards_remote_audio_without_semantic_reinterpretation() {
        let (audio_tx, mut audio_rx) = mpsc::channel(4);
        let (event_tx, _event_rx) = mpsc::channel(4);
        let session_id = Uuid::new_v4();
        let bridge = LiveKitSessionBridge::new(session_id, audio_tx, event_tx);
        let frame = AudioFrame::mono_16khz(1, vec![1, 2, 3, 4]);

        bridge
            .handle_event(LiveKitEvent::RemoteAudio(frame.clone()))
            .await
            .unwrap();

        assert_eq!(audio_rx.recv().await, Some(frame));
    }

    #[tokio::test]
    async fn disconnect_emits_session_ended() {
        let (audio_tx, _audio_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let session_id = Uuid::new_v4();
        let bridge = LiveKitSessionBridge::new(session_id, audio_tx, event_tx);

        bridge.handle_event(LiveKitEvent::Disconnected).await.unwrap();

        assert_eq!(
            event_rx.recv().await,
            Some(VoiceEvent::SessionEnded { session_id })
        );
    }
}

use nulang_ai_core::voice::{AudioFrame, SpeechSynthesisStream, VoiceEvent, VoiceProviderError};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Owns the active TTS stream for a voice session and makes interruption a
/// first-class operation. VAD/turn logic can call `interrupt` as soon as new
/// user speech begins, deterministically cancelling synthesized output.
pub struct PlaybackController {
    session_id: Uuid,
    current: Option<Box<dyn SpeechSynthesisStream>>,
    event_tx: mpsc::Sender<VoiceEvent>,
}

impl PlaybackController {
    pub fn new(session_id: Uuid, event_tx: mpsc::Sender<VoiceEvent>) -> Self {
        Self {
            session_id,
            current: None,
            event_tx,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.current.is_some()
    }

    pub async fn start(
        &mut self,
        stream: Box<dyn SpeechSynthesisStream>,
    ) -> Result<(), PlaybackError> {
        if let Some(mut current) = self.current.take() {
            current.cancel().await.map_err(PlaybackError::Provider)?;
        }
        self.current = Some(stream);
        self.event_tx
            .send(VoiceEvent::ResponseStarted {
                session_id: self.session_id,
            })
            .await
            .map_err(|_| PlaybackError::EventChannelClosed)
    }

    pub async fn next_frame(&mut self) -> Result<Option<AudioFrame>, PlaybackError> {
        let Some(stream) = self.current.as_mut() else {
            return Ok(None);
        };

        match stream.next_frame().await.map_err(PlaybackError::Provider)? {
            Some(frame) => Ok(Some(frame)),
            None => {
                self.current = None;
                Ok(None)
            }
        }
    }

    pub async fn interrupt(&mut self) -> Result<bool, PlaybackError> {
        let Some(mut stream) = self.current.take() else {
            return Ok(false);
        };

        stream.cancel().await.map_err(PlaybackError::Provider)?;
        self.event_tx
            .send(VoiceEvent::ResponseInterrupted {
                session_id: self.session_id,
            })
            .await
            .map_err(|_| PlaybackError::EventChannelClosed)?;
        Ok(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackError {
    Provider(VoiceProviderError),
    EventChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::voice::VoiceFuture;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    struct FakeStream {
        cancelled: Arc<AtomicBool>,
    }

    impl SpeechSynthesisStream for FakeStream {
        fn next_frame<'a>(
            &'a mut self,
        ) -> VoiceFuture<'a, Result<Option<AudioFrame>, VoiceProviderError>> {
            Box::pin(async { Ok(Some(AudioFrame::mono_16khz(0, vec![1; 160]))) })
        }

        fn cancel<'a>(&'a mut self) -> VoiceFuture<'a, Result<(), VoiceProviderError>> {
            Box::pin(async move {
                self.cancelled.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn interrupt_cancels_tts_and_emits_event() {
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let session_id = Uuid::new_v4();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut playback = PlaybackController::new(session_id, event_tx);

        playback
            .start(Box::new(FakeStream {
                cancelled: cancelled.clone(),
            }))
            .await
            .unwrap();
        assert_eq!(
            event_rx.recv().await,
            Some(VoiceEvent::ResponseStarted { session_id })
        );

        assert!(playback.interrupt().await.unwrap());
        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(
            event_rx.recv().await,
            Some(VoiceEvent::ResponseInterrupted { session_id })
        );
    }
}

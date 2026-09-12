use nulang_ai_core::voice::{
    AudioFrame, RecognitionConfig, SpeechRecognitionSession, SpeechRecognizer, Transcript,
    VoiceFuture, VoiceProviderError,
};
use std::sync::Arc;

/// Provider/runtime seam for Parakeet. Implementations may use a local model,
/// a GPU worker, or a sidecar without coupling nulang-ai-voice to CUDA/TensorRT.
pub trait ParakeetRuntime: Send + Sync {
    fn start<'a>(
        &'a self,
        config: RecognitionConfig,
    ) -> VoiceFuture<'a, Result<Box<dyn ParakeetRuntimeSession>, VoiceProviderError>>;
}

pub trait ParakeetRuntimeSession: Send {
    fn push<'a>(
        &'a mut self,
        frame: AudioFrame,
    ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>>;

    fn finish<'a>(&'a mut self) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>>;
}

pub struct ParakeetRecognizer<R> {
    runtime: Arc<R>,
}

impl<R> ParakeetRecognizer<R>
where
    R: ParakeetRuntime,
{
    pub fn new(runtime: R) -> Self {
        Self {
            runtime: Arc::new(runtime),
        }
    }
}

struct ParakeetSession {
    inner: Box<dyn ParakeetRuntimeSession>,
}

impl SpeechRecognitionSession for ParakeetSession {
    fn push_audio<'a>(
        &'a mut self,
        frame: AudioFrame,
    ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>> {
        self.inner.push(frame)
    }

    fn finish<'a>(&'a mut self) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>> {
        self.inner.finish()
    }
}

impl<R> SpeechRecognizer for ParakeetRecognizer<R>
where
    R: ParakeetRuntime + 'static,
{
    fn provider_name(&self) -> &str {
        "parakeet"
    }

    fn start_session<'a>(
        &'a self,
        config: RecognitionConfig,
    ) -> VoiceFuture<'a, Result<Box<dyn SpeechRecognitionSession>, VoiceProviderError>> {
        Box::pin(async move {
            let inner = self.runtime.start(config).await?;
            Ok(Box::new(ParakeetSession { inner }) as Box<dyn SpeechRecognitionSession>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::voice::TranscriptKind;

    struct FakeRuntime;
    struct FakeSession;

    impl ParakeetRuntime for FakeRuntime {
        fn start<'a>(
            &'a self,
            _config: RecognitionConfig,
        ) -> VoiceFuture<'a, Result<Box<dyn ParakeetRuntimeSession>, VoiceProviderError>> {
            Box::pin(async { Ok(Box::new(FakeSession) as Box<dyn ParakeetRuntimeSession>) })
        }
    }

    impl ParakeetRuntimeSession for FakeSession {
        fn push<'a>(
            &'a mut self,
            _frame: AudioFrame,
        ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>> {
            Box::pin(async {
                Ok(vec![Transcript {
                    text: "ship the voice adapter".into(),
                    kind: TranscriptKind::Partial,
                    confidence: Some(0.95),
                    language: Some("en".into()),
                    started_at_ms: Some(0),
                    ended_at_ms: Some(500),
                }])
            })
        }

        fn finish<'a>(
            &'a mut self,
        ) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[tokio::test]
    async fn streams_partial_transcripts() {
        let recognizer = ParakeetRecognizer::new(FakeRuntime);
        let mut session = recognizer
            .start_session(RecognitionConfig::default())
            .await
            .unwrap();
        let transcripts = session
            .push_audio(AudioFrame::mono_16khz(0, vec![0; 160]))
            .await
            .unwrap();
        assert_eq!(transcripts.len(), 1);
        assert_eq!(recognizer.provider_name(), "parakeet");
    }
}

use nulang_ai_core::voice::{
    AudioFrame, RecognitionConfig, SpeechRecognitionSession, SpeechRecognizer, Transcript,
    VoiceFuture, VoiceProviderError,
};
use std::sync::Arc;

/// Runtime seam for Whisper/faster-whisper/whisper.cpp implementations.
pub trait WhisperRuntime: Send + Sync {
    fn start<'a>(
        &'a self,
        config: RecognitionConfig,
    ) -> VoiceFuture<'a, Result<Box<dyn WhisperRuntimeSession>, VoiceProviderError>>;
}

pub trait WhisperRuntimeSession: Send {
    fn push<'a>(
        &'a mut self,
        frame: AudioFrame,
    ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>>;

    fn finish<'a>(
        &'a mut self,
    ) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>>;
}

pub struct WhisperRecognizer<R> {
    runtime: Arc<R>,
}

impl<R> WhisperRecognizer<R>
where
    R: WhisperRuntime,
{
    pub fn new(runtime: R) -> Self {
        Self {
            runtime: Arc::new(runtime),
        }
    }
}

struct WhisperSession {
    inner: Box<dyn WhisperRuntimeSession>,
}

impl SpeechRecognitionSession for WhisperSession {
    fn push_audio<'a>(
        &'a mut self,
        frame: AudioFrame,
    ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>> {
        self.inner.push(frame)
    }

    fn finish<'a>(
        &'a mut self,
    ) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>> {
        self.inner.finish()
    }
}

impl<R> SpeechRecognizer for WhisperRecognizer<R>
where
    R: WhisperRuntime + 'static,
{
    fn provider_name(&self) -> &str {
        "whisper"
    }

    fn start_session<'a>(
        &'a self,
        config: RecognitionConfig,
    ) -> VoiceFuture<'a, Result<Box<dyn SpeechRecognitionSession>, VoiceProviderError>> {
        Box::pin(async move {
            let inner = self.runtime.start(config).await?;
            Ok(Box::new(WhisperSession { inner }) as Box<dyn SpeechRecognitionSession>)
        })
    }
}

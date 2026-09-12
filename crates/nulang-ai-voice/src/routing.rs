use nulang_ai_core::voice::{
    RecognitionConfig, SpeechRecognitionSession, SpeechRecognizer, VoiceFuture, VoiceProviderError,
};
use std::sync::Arc;

/// Routes English realtime recognition to Parakeet and multilingual sessions
/// to Whisper, while falling back to Whisper if Parakeet cannot start.
pub struct AutoSpeechRecognizer {
    parakeet: Arc<dyn SpeechRecognizer>,
    whisper: Arc<dyn SpeechRecognizer>,
}

impl AutoSpeechRecognizer {
    pub fn new(
        parakeet: Arc<dyn SpeechRecognizer>,
        whisper: Arc<dyn SpeechRecognizer>,
    ) -> Self {
        Self { parakeet, whisper }
    }

    fn prefer_parakeet(config: &RecognitionConfig) -> bool {
        config
            .language
            .as_deref()
            .map(|lang| lang.eq_ignore_ascii_case("en") || lang.to_ascii_lowercase().starts_with("en-"))
            .unwrap_or(true)
    }
}

impl SpeechRecognizer for AutoSpeechRecognizer {
    fn provider_name(&self) -> &str {
        "auto"
    }

    fn start_session<'a>(
        &'a self,
        config: RecognitionConfig,
    ) -> VoiceFuture<'a, Result<Box<dyn SpeechRecognitionSession>, VoiceProviderError>> {
        Box::pin(async move {
            if Self::prefer_parakeet(&config) {
                match self.parakeet.start_session(config.clone()).await {
                    Ok(session) => Ok(session),
                    Err(_) => self.whisper.start_session(config).await,
                }
            } else {
                self.whisper.start_session(config).await
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::voice::{AudioFrame, Transcript};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeRecognizer {
        name: &'static str,
        starts: Arc<AtomicUsize>,
        fail: bool,
    }

    struct FakeSession;

    impl SpeechRecognitionSession for FakeSession {
        fn push_audio<'a>(
            &'a mut self,
            _frame: AudioFrame,
        ) -> VoiceFuture<'a, Result<Vec<Transcript>, VoiceProviderError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn finish<'a>(
            &'a mut self,
        ) -> VoiceFuture<'a, Result<Option<Transcript>, VoiceProviderError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl SpeechRecognizer for FakeRecognizer {
        fn provider_name(&self) -> &str {
            self.name
        }

        fn start_session<'a>(
            &'a self,
            _config: RecognitionConfig,
        ) -> VoiceFuture<'a, Result<Box<dyn SpeechRecognitionSession>, VoiceProviderError>> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.fail {
                    Err(VoiceProviderError::new(self.name, "unavailable", "down", true))
                } else {
                    Ok(Box::new(FakeSession) as Box<dyn SpeechRecognitionSession>)
                }
            })
        }
    }

    #[tokio::test]
    async fn uses_whisper_for_non_english() {
        let p = Arc::new(AtomicUsize::new(0));
        let w = Arc::new(AtomicUsize::new(0));
        let router = AutoSpeechRecognizer::new(
            Arc::new(FakeRecognizer { name: "parakeet", starts: p.clone(), fail: false }),
            Arc::new(FakeRecognizer { name: "whisper", starts: w.clone(), fail: false }),
        );
        let mut config = RecognitionConfig::default();
        config.language = Some("pt-BR".into());
        router.start_session(config).await.unwrap();
        assert_eq!(p.load(Ordering::SeqCst), 0);
        assert_eq!(w.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn falls_back_when_parakeet_start_fails() {
        let p = Arc::new(AtomicUsize::new(0));
        let w = Arc::new(AtomicUsize::new(0));
        let router = AutoSpeechRecognizer::new(
            Arc::new(FakeRecognizer { name: "parakeet", starts: p.clone(), fail: true }),
            Arc::new(FakeRecognizer { name: "whisper", starts: w.clone(), fail: false }),
        );
        router.start_session(RecognitionConfig::default()).await.unwrap();
        assert_eq!(p.load(Ordering::SeqCst), 1);
        assert_eq!(w.load(Ordering::SeqCst), 1);
    }
}

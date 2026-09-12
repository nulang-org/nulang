use nulang_ai_core::voice::{
    AudioFrame, SpeechRequest, SpeechSynthesisStream, SpeechSynthesizer, VoiceFuture,
    VoiceProviderError,
};
use std::sync::Arc;

/// Runtime seam for local Kokoro implementations or a remote synthesis worker.
pub trait KokoroRuntime: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
    ) -> VoiceFuture<'a, Result<Box<dyn KokoroRuntimeStream>, VoiceProviderError>>;
}

pub trait KokoroRuntimeStream: Send {
    fn next_frame<'a>(
        &'a mut self,
    ) -> VoiceFuture<'a, Result<Option<AudioFrame>, VoiceProviderError>>;

    fn cancel<'a>(&'a mut self) -> VoiceFuture<'a, Result<(), VoiceProviderError>>;
}

pub struct KokoroSynthesizer<R> {
    runtime: Arc<R>,
}

impl<R> KokoroSynthesizer<R>
where
    R: KokoroRuntime,
{
    pub fn new(runtime: R) -> Self {
        Self {
            runtime: Arc::new(runtime),
        }
    }
}

struct KokoroStream {
    inner: Box<dyn KokoroRuntimeStream>,
}

impl SpeechSynthesisStream for KokoroStream {
    fn next_frame<'a>(
        &'a mut self,
    ) -> VoiceFuture<'a, Result<Option<AudioFrame>, VoiceProviderError>> {
        self.inner.next_frame()
    }

    fn cancel<'a>(&'a mut self) -> VoiceFuture<'a, Result<(), VoiceProviderError>> {
        self.inner.cancel()
    }
}

impl<R> SpeechSynthesizer for KokoroSynthesizer<R>
where
    R: KokoroRuntime + 'static,
{
    fn provider_name(&self) -> &str {
        "kokoro"
    }

    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
    ) -> VoiceFuture<'a, Result<Box<dyn SpeechSynthesisStream>, VoiceProviderError>> {
        Box::pin(async move {
            let inner = self.runtime.synthesize(request).await?;
            Ok(Box::new(KokoroStream { inner }) as Box<dyn SpeechSynthesisStream>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeRuntime {
        cancelled: Arc<AtomicBool>,
    }

    struct FakeStream {
        cancelled: Arc<AtomicBool>,
        emitted: bool,
    }

    impl KokoroRuntime for FakeRuntime {
        fn synthesize<'a>(
            &'a self,
            _request: SpeechRequest,
        ) -> VoiceFuture<'a, Result<Box<dyn KokoroRuntimeStream>, VoiceProviderError>> {
            let cancelled = self.cancelled.clone();
            Box::pin(async move {
                Ok(Box::new(FakeStream {
                    cancelled,
                    emitted: false,
                }) as Box<dyn KokoroRuntimeStream>)
            })
        }
    }

    impl KokoroRuntimeStream for FakeStream {
        fn next_frame<'a>(
            &'a mut self,
        ) -> VoiceFuture<'a, Result<Option<AudioFrame>, VoiceProviderError>> {
            Box::pin(async move {
                if self.emitted {
                    Ok(None)
                } else {
                    self.emitted = true;
                    Ok(Some(AudioFrame::mono_16khz(0, vec![1; 160])))
                }
            })
        }

        fn cancel<'a>(&'a mut self) -> VoiceFuture<'a, Result<(), VoiceProviderError>> {
            Box::pin(async move {
                self.cancelled.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn forwards_stream_and_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let tts = KokoroSynthesizer::new(FakeRuntime {
            cancelled: cancelled.clone(),
        });
        let mut stream = tts.synthesize(SpeechRequest::new("hello")).await.unwrap();
        assert!(stream.next_frame().await.unwrap().is_some());
        stream.cancel().await.unwrap();
        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(tts.provider_name(), "kokoro");
    }
}

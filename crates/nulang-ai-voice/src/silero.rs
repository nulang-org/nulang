use nulang_ai_core::voice::{
    AudioFrame, VoiceActivity, VoiceActivityDetector, VoiceFuture, VoiceProviderError,
};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SileroVadConfig {
    pub speech_threshold: f32,
    pub min_speech_ms: u32,
    pub min_silence_ms: u32,
}

impl Default for SileroVadConfig {
    fn default() -> Self {
        Self {
            speech_threshold: 0.5,
            min_speech_ms: 120,
            min_silence_ms: 250,
        }
    }
}

/// Minimal inference seam for an ONNX/ORT runtime, Python sidecar, or native
/// Silero implementation. The model runner stays outside this crate so NuLang
/// does not force heavyweight inference dependencies on every runtime build.
pub trait SileroVadRuntime: Send + Sync {
    fn speech_probability<'a>(
        &'a self,
        frame: &'a AudioFrame,
    ) -> VoiceFuture<'a, Result<f32, VoiceProviderError>>;
}

pub struct SileroVadAdapter<R> {
    runtime: Arc<R>,
    config: SileroVadConfig,
}

impl<R> SileroVadAdapter<R>
where
    R: SileroVadRuntime,
{
    pub fn new(runtime: R, config: SileroVadConfig) -> Self {
        Self {
            runtime: Arc::new(runtime),
            config,
        }
    }

    pub fn config(&self) -> SileroVadConfig {
        self.config
    }
}

impl<R> VoiceActivityDetector for SileroVadAdapter<R>
where
    R: SileroVadRuntime + 'static,
{
    fn provider_name(&self) -> &str {
        "silero"
    }

    fn detect<'a>(
        &'a self,
        frame: &'a AudioFrame,
    ) -> VoiceFuture<'a, Result<VoiceActivity, VoiceProviderError>> {
        Box::pin(async move {
            let probability = self.runtime.speech_probability(frame).await?;
            Ok(if probability >= self.config.speech_threshold {
                VoiceActivity::Speech
            } else {
                VoiceActivity::Silence
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRuntime(f32);

    impl SileroVadRuntime for FakeRuntime {
        fn speech_probability<'a>(
            &'a self,
            _frame: &'a AudioFrame,
        ) -> VoiceFuture<'a, Result<f32, VoiceProviderError>> {
            Box::pin(async move { Ok(self.0) })
        }
    }

    #[tokio::test]
    async fn classifies_speech_using_configured_threshold() {
        let vad = SileroVadAdapter::new(FakeRuntime(0.9), SileroVadConfig::default());
        let activity = vad
            .detect(&AudioFrame::mono_16khz(0, vec![0; 160]))
            .await
            .unwrap();
        assert_eq!(activity, VoiceActivity::Speech);
        assert_eq!(vad.provider_name(), "silero");
    }
}

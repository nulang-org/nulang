use async_trait::async_trait;
use nulang_ai_core::voice::{AudioFrame, VoiceActivity, VoiceActivityDetector, VoiceError};
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

/// Minimal runtime seam for an ONNX/ORT, Python sidecar, or native Silero VAD
/// implementation. Keeping model execution out of this crate avoids forcing a
/// heavyweight inference dependency on every NuLang runtime build.
#[async_trait]
pub trait SileroVadRuntime: Send + Sync {
    async fn speech_probability(&self, frame: &AudioFrame) -> Result<f32, VoiceError>;
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

#[async_trait]
impl<R> VoiceActivityDetector for SileroVadAdapter<R>
where
    R: SileroVadRuntime + 'static,
{
    async fn detect(&self, frame: &AudioFrame) -> Result<VoiceActivity, VoiceError> {
        let probability = self.runtime.speech_probability(frame).await?;
        Ok(if probability >= self.config.speech_threshold {
            VoiceActivity::Speech { probability }
        } else {
            VoiceActivity::Silence { probability }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::voice::{AudioEncoding, AudioFrame};

    struct FakeRuntime(f32);

    #[async_trait]
    impl SileroVadRuntime for FakeRuntime {
        async fn speech_probability(&self, _frame: &AudioFrame) -> Result<f32, VoiceError> {
            Ok(self.0)
        }
    }

    fn frame() -> AudioFrame {
        AudioFrame {
            sequence: 0,
            sample_rate_hz: 16_000,
            channels: 1,
            encoding: AudioEncoding::PcmS16Le,
            data: vec![0; 320],
        }
    }

    #[tokio::test]
    async fn classifies_speech_using_configured_threshold() {
        let vad = SileroVadAdapter::new(FakeRuntime(0.9), SileroVadConfig::default());
        let activity = vad.detect(&frame()).await.unwrap();
        assert!(matches!(activity, VoiceActivity::Speech { .. }));
    }
}

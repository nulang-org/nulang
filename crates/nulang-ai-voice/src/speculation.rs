use nulang_ai_core::voice::{Transcript, TranscriptKind, VoiceFuture, VoiceProviderError};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchSafety {
    /// Queries, repository searches, reads, and other operations with no
    /// externally visible mutation are safe to start speculatively.
    ReadOnly,
    /// Reserved for future compensating transactions. The initial controller
    /// intentionally refuses these while speech is still partial.
    Reversible,
    /// Never allowed from a partial transcript.
    Irreversible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeculationHandle {
    pub id: Uuid,
    pub transcript: String,
}

impl SpeculationHandle {
    pub fn new(transcript: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            transcript: transcript.into(),
        }
    }
}

pub trait SpeculativeIntentRuntime: Send + Sync {
    /// Start work that is guaranteed to be read-only. Implementations may do
    /// retrieval, repository search, parsing, planning, or cache warming, but
    /// must not commit user-visible side effects.
    fn start_read_only<'a>(
        &'a self,
        transcript: &'a str,
    ) -> VoiceFuture<'a, Result<SpeculationHandle, VoiceProviderError>>;

    fn cancel<'a>(
        &'a self,
        handle: &'a SpeculationHandle,
    ) -> VoiceFuture<'a, Result<(), VoiceProviderError>>;

    /// Promote reusable speculative results after the final transcript has
    /// confirmed that the partial intent remained compatible. This operation
    /// only promotes cached/read-only work; normal capability checks still
    /// govern any eventual side effect.
    fn reuse<'a>(
        &'a self,
        handle: &'a SpeculationHandle,
        final_transcript: &'a str,
    ) -> VoiceFuture<'a, Result<(), VoiceProviderError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeculationOutcome {
    Started,
    Replaced,
    Reused,
    Cancelled,
    Ignored,
}

pub struct SpeculativeIntentController<R> {
    runtime: Arc<R>,
    active: Option<SpeculationHandle>,
}

impl<R> SpeculativeIntentController<R>
where
    R: SpeculativeIntentRuntime,
{
    pub fn new(runtime: R) -> Self {
        Self {
            runtime: Arc::new(runtime),
            active: None,
        }
    }

    pub fn active(&self) -> Option<&SpeculationHandle> {
        self.active.as_ref()
    }

    pub async fn observe(
        &mut self,
        transcript: &Transcript,
        safety: PrefetchSafety,
    ) -> Result<SpeculationOutcome, VoiceProviderError> {
        match transcript.kind {
            TranscriptKind::Partial => self.observe_partial(&transcript.text, safety).await,
            TranscriptKind::Final => self.observe_final(&transcript.text).await,
        }
    }

    pub async fn observe_partial(
        &mut self,
        transcript: &str,
        safety: PrefetchSafety,
    ) -> Result<SpeculationOutcome, VoiceProviderError> {
        let normalized = normalize(transcript);
        if normalized.is_empty() || safety != PrefetchSafety::ReadOnly {
            return Ok(SpeculationOutcome::Ignored);
        }

        if self
            .active
            .as_ref()
            .is_some_and(|active| normalize(&active.transcript) == normalized)
        {
            return Ok(SpeculationOutcome::Ignored);
        }

        // Do not drop the local handle until the runtime confirms cancellation.
        // If cancellation fails, retaining it lets callers retry instead of
        // orphaning speculative work that may still be running remotely.
        let replaced = if let Some(previous) = self.active.clone() {
            self.runtime.cancel(&previous).await?;
            self.active = None;
            true
        } else {
            false
        };

        let handle = self.runtime.start_read_only(transcript).await?;
        self.active = Some(handle);
        Ok(if replaced {
            SpeculationOutcome::Replaced
        } else {
            SpeculationOutcome::Started
        })
    }

    pub async fn observe_final(
        &mut self,
        transcript: &str,
    ) -> Result<SpeculationOutcome, VoiceProviderError> {
        let Some(active) = self.active.clone() else {
            return Ok(SpeculationOutcome::Ignored);
        };

        if compatible_partial(&active.transcript, transcript) {
            self.runtime.reuse(&active, transcript).await?;
            self.active = None;
            Ok(SpeculationOutcome::Reused)
        } else {
            self.runtime.cancel(&active).await?;
            self.active = None;
            Ok(SpeculationOutcome::Cancelled)
        }
    }

    pub async fn cancel_active(&mut self) -> Result<bool, VoiceProviderError> {
        let Some(active) = self.active.clone() else {
            return Ok(false);
        };
        self.runtime.cancel(&active).await?;
        self.active = None;
        Ok(true)
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_ascii_lowercase()
}

fn compatible_partial(partial: &str, final_transcript: &str) -> bool {
    let partial = normalize(partial);
    let final_transcript = normalize(final_transcript);
    !partial.is_empty()
        && (final_transcript == partial
            || final_transcript
                .strip_prefix(&partial)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(' ')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRuntime {
        started: Mutex<Vec<String>>,
        cancelled: Mutex<Vec<Uuid>>,
        reused: Mutex<Vec<Uuid>>,
        fail_cancel: bool,
        fail_reuse: bool,
    }

    impl SpeculativeIntentRuntime for FakeRuntime {
        fn start_read_only<'a>(
            &'a self,
            transcript: &'a str,
        ) -> VoiceFuture<'a, Result<SpeculationHandle, VoiceProviderError>> {
            Box::pin(async move {
                self.started.lock().unwrap().push(transcript.to_string());
                Ok(SpeculationHandle::new(transcript))
            })
        }

        fn cancel<'a>(
            &'a self,
            handle: &'a SpeculationHandle,
        ) -> VoiceFuture<'a, Result<(), VoiceProviderError>> {
            Box::pin(async move {
                if self.fail_cancel {
                    return Err(VoiceProviderError::new(
                        "fake",
                        "cancel_failed",
                        "simulated cancellation failure",
                        true,
                    ));
                }
                self.cancelled.lock().unwrap().push(handle.id);
                Ok(())
            })
        }

        fn reuse<'a>(
            &'a self,
            handle: &'a SpeculationHandle,
            _final_transcript: &'a str,
        ) -> VoiceFuture<'a, Result<(), VoiceProviderError>> {
            Box::pin(async move {
                if self.fail_reuse {
                    return Err(VoiceProviderError::new(
                        "fake",
                        "reuse_failed",
                        "simulated reuse failure",
                        true,
                    ));
                }
                self.reused.lock().unwrap().push(handle.id);
                Ok(())
            })
        }
    }

    fn transcript(text: &str, kind: TranscriptKind) -> Transcript {
        Transcript {
            text: text.into(),
            kind,
            confidence: None,
            language: Some("en".into()),
            started_at_ms: None,
            ended_at_ms: None,
        }
    }

    #[tokio::test]
    async fn test_partial_starts_read_only_prefetch() {
        let mut controller = SpeculativeIntentController::new(FakeRuntime::default());
        let result = controller
            .observe(
                &transcript("search the auth code", TranscriptKind::Partial),
                PrefetchSafety::ReadOnly,
            )
            .await
            .unwrap();
        assert_eq!(result, SpeculationOutcome::Started);
        assert!(controller.active().is_some());
    }

    #[tokio::test]
    async fn test_changed_partial_replaces_stale_prefetch() {
        let mut controller = SpeculativeIntentController::new(FakeRuntime::default());
        controller
            .observe_partial("search auth", PrefetchSafety::ReadOnly)
            .await
            .unwrap();
        let result = controller
            .observe_partial("search auth middleware", PrefetchSafety::ReadOnly)
            .await
            .unwrap();
        assert_eq!(result, SpeculationOutcome::Replaced);
    }

    #[tokio::test]
    async fn test_compatible_final_reuses_prefetch() {
        let mut controller = SpeculativeIntentController::new(FakeRuntime::default());
        controller
            .observe_partial("review the auth", PrefetchSafety::ReadOnly)
            .await
            .unwrap();
        let result = controller
            .observe_final("review the auth code")
            .await
            .unwrap();
        assert_eq!(result, SpeculationOutcome::Reused);
        assert!(controller.active().is_none());
    }

    #[tokio::test]
    async fn test_incompatible_final_cancels_prefetch() {
        let mut controller = SpeculativeIntentController::new(FakeRuntime::default());
        controller
            .observe_partial("review billing", PrefetchSafety::ReadOnly)
            .await
            .unwrap();
        let result = controller.observe_final("delete billing account").await.unwrap();
        assert_eq!(result, SpeculationOutcome::Cancelled);
    }

    #[tokio::test]
    async fn test_irreversible_partial_is_refused() {
        let mut controller = SpeculativeIntentController::new(FakeRuntime::default());
        let result = controller
            .observe_partial("delete production", PrefetchSafety::Irreversible)
            .await
            .unwrap();
        assert_eq!(result, SpeculationOutcome::Ignored);
        assert!(controller.active().is_none());
    }

    #[tokio::test]
    async fn test_cancel_failure_retains_active_handle_for_retry() {
        let runtime = FakeRuntime {
            fail_cancel: true,
            ..FakeRuntime::default()
        };
        let mut controller = SpeculativeIntentController::new(runtime);
        controller
            .observe_partial("search auth", PrefetchSafety::ReadOnly)
            .await
            .unwrap();
        let active_id = controller.active().unwrap().id;

        let error = controller.cancel_active().await.unwrap_err();

        assert_eq!(error.code, "cancel_failed");
        assert_eq!(controller.active().map(|handle| handle.id), Some(active_id));
    }

    #[tokio::test]
    async fn test_reuse_failure_retains_active_handle_for_retry_or_cancel() {
        let runtime = FakeRuntime {
            fail_reuse: true,
            ..FakeRuntime::default()
        };
        let mut controller = SpeculativeIntentController::new(runtime);
        controller
            .observe_partial("review the auth", PrefetchSafety::ReadOnly)
            .await
            .unwrap();
        let active_id = controller.active().unwrap().id;

        let error = controller
            .observe_final("review the auth code")
            .await
            .unwrap_err();

        assert_eq!(error.code, "reuse_failed");
        assert_eq!(controller.active().map(|handle| handle.id), Some(active_id));
    }
}

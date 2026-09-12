use nulang_ai_core::voice::{TranscriptKind, VoiceActivity};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnDetectorConfig {
    /// Silence required after an STT-final transcript.
    pub final_silence_ms: u64,
    /// Silence required when STT still considers the transcript partial.
    pub partial_silence_ms: u64,
    /// Minimum semantic-completion score required to close a partial turn.
    pub semantic_completion_threshold: f32,
    /// Hard safety bound for turns that already contain transcript text.
    pub max_turn_ms: u64,
}

impl Default for TurnDetectorConfig {
    fn default() -> Self {
        Self {
            final_silence_ms: 320,
            partial_silence_ms: 800,
            semantic_completion_threshold: 0.78,
            max_turn_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnObservation {
    pub activity: VoiceActivity,
    pub transcript_kind: Option<TranscriptKind>,
    /// Consecutive silence observed by the VAD.
    pub silence_ms: u64,
    /// Time since speech for this turn first began.
    pub elapsed_ms: u64,
    /// Optional provider/model score in [0, 1] estimating whether the current
    /// utterance is semantically complete. Absence never implies completion.
    pub semantic_completion: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCompletionReason {
    FinalTranscriptSilence,
    SemanticPartialSilence,
    MaxDuration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    Continue,
    Complete(TurnCompletionReason),
}

/// Combines VAD, STT state, and an optional semantic-completion signal.
///
/// Silence alone never completes a turn. This deliberately prevents a VAD
/// pause from being mistaken for the end of a thought.
pub struct SemanticTurnDetector {
    config: TurnDetectorConfig,
}

impl SemanticTurnDetector {
    pub fn new(config: TurnDetectorConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> TurnDetectorConfig {
        self.config
    }

    pub fn evaluate(&self, observation: TurnObservation) -> TurnDecision {
        if observation.activity == VoiceActivity::Speech {
            return TurnDecision::Continue;
        }

        let Some(kind) = observation.transcript_kind else {
            return TurnDecision::Continue;
        };

        if observation.elapsed_ms >= self.config.max_turn_ms {
            return TurnDecision::Complete(TurnCompletionReason::MaxDuration);
        }

        match kind {
            TranscriptKind::Final if observation.silence_ms >= self.config.final_silence_ms => {
                TurnDecision::Complete(TurnCompletionReason::FinalTranscriptSilence)
            }
            TranscriptKind::Partial
                if observation.silence_ms >= self.config.partial_silence_ms
                    && observation
                        .semantic_completion
                        .is_some_and(|score| score >= self.config.semantic_completion_threshold) =>
            {
                TurnDecision::Complete(TurnCompletionReason::SemanticPartialSilence)
            }
            _ => TurnDecision::Continue,
        }
    }
}

impl Default for SemanticTurnDetector {
    fn default() -> Self {
        Self::new(TurnDetectorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(kind: Option<TranscriptKind>, silence_ms: u64) -> TurnObservation {
        TurnObservation {
            activity: VoiceActivity::Silence,
            transcript_kind: kind,
            silence_ms,
            elapsed_ms: 2_000,
            semantic_completion: None,
        }
    }

    #[test]
    fn test_silence_without_transcript_never_completes_turn() {
        let detector = SemanticTurnDetector::default();
        assert_eq!(
            detector.evaluate(observation(None, 10_000)),
            TurnDecision::Continue
        );
    }

    #[test]
    fn test_final_transcript_completes_after_short_silence() {
        let detector = SemanticTurnDetector::default();
        assert_eq!(
            detector.evaluate(observation(Some(TranscriptKind::Final), 320)),
            TurnDecision::Complete(TurnCompletionReason::FinalTranscriptSilence)
        );
    }

    #[test]
    fn test_partial_requires_semantic_completion_and_longer_silence() {
        let detector = SemanticTurnDetector::default();
        let mut obs = observation(Some(TranscriptKind::Partial), 900);
        assert_eq!(detector.evaluate(obs), TurnDecision::Continue);

        obs.semantic_completion = Some(0.9);
        assert_eq!(
            detector.evaluate(obs),
            TurnDecision::Complete(TurnCompletionReason::SemanticPartialSilence)
        );
    }

    #[test]
    fn test_active_speech_never_completes_turn() {
        let detector = SemanticTurnDetector::default();
        let mut obs = observation(Some(TranscriptKind::Final), 1_000);
        obs.activity = VoiceActivity::Speech;
        assert_eq!(detector.evaluate(obs), TurnDecision::Continue);
    }

    #[test]
    fn test_max_duration_requires_a_transcript() {
        let detector = SemanticTurnDetector::default();
        let mut obs = observation(Some(TranscriptKind::Partial), 0);
        obs.elapsed_ms = 30_000;
        assert_eq!(
            detector.evaluate(obs),
            TurnDecision::Complete(TurnCompletionReason::MaxDuration)
        );
    }
}

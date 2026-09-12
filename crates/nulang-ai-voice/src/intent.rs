use nulang_ai_core::{
    intent::{IntentIr, IntentModality, IntentPhase, IntentSafety},
    voice::{Transcript, TranscriptKind},
};
use uuid::Uuid;

/// Converts speech recognition output into the modality-neutral Intent IR.
///
/// Partial transcripts always become provisional intents and are constrained to
/// read-only speculation. Final transcripts become confirmed intents, but their
/// safety classification remains unknown until the policy/capability layer
/// classifies the requested operation.
pub struct VoiceIntentBridge {
    conversation_id: Option<Uuid>,
    session_id: Uuid,
}

impl VoiceIntentBridge {
    pub fn new(session_id: Uuid, conversation_id: Option<Uuid>) -> Self {
        Self {
            conversation_id,
            session_id,
        }
    }

    pub fn to_intent(&self, transcript: &Transcript) -> IntentIr {
        let phase = match transcript.kind {
            TranscriptKind::Partial => IntentPhase::Provisional,
            TranscriptKind::Final => IntentPhase::Confirmed,
        };

        let mut intent = IntentIr::new(IntentModality::Voice, phase, transcript.text.clone());
        intent.conversation_id = self.conversation_id;
        intent.source_session_id = Some(self.session_id);
        intent.language = transcript.language.clone();
        intent.confidence = transcript.confidence;
        intent.safety = match transcript.kind {
            TranscriptKind::Partial => IntentSafety::ReadOnly,
            TranscriptKind::Final => IntentSafety::Unknown,
        };
        intent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(kind: TranscriptKind) -> Transcript {
        Transcript {
            text: "review the auth module".into(),
            kind,
            confidence: Some(0.98),
            language: Some("en".into()),
            started_at_ms: Some(0),
            ended_at_ms: Some(750),
        }
    }

    #[test]
    fn partial_voice_transcript_becomes_read_only_provisional_intent() {
        let session_id = Uuid::new_v4();
        let bridge = VoiceIntentBridge::new(session_id, None);
        let intent = bridge.to_intent(&transcript(TranscriptKind::Partial));

        assert_eq!(intent.phase, IntentPhase::Provisional);
        assert_eq!(intent.safety, IntentSafety::ReadOnly);
        assert_eq!(intent.source_session_id, Some(session_id));
        assert!(!intent.is_executable());
    }

    #[test]
    fn final_voice_transcript_becomes_confirmed_but_unclassified_intent() {
        let conversation_id = Uuid::new_v4();
        let bridge = VoiceIntentBridge::new(Uuid::new_v4(), Some(conversation_id));
        let intent = bridge.to_intent(&transcript(TranscriptKind::Final));

        assert_eq!(intent.phase, IntentPhase::Confirmed);
        assert_eq!(intent.safety, IntentSafety::Unknown);
        assert_eq!(intent.execution_risk, None);
        assert_eq!(intent.conversation_id, Some(conversation_id));
        assert!(!intent.is_executable());
    }
}

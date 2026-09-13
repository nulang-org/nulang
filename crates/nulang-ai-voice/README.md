# nulang-ai-voice

Provider-neutral realtime voice primitives and adapters for the NuLang agent runtime.

## Pipeline

`LiveKit/WebRTC -> Silero VAD -> Parakeet/Whisper -> semantic turn detection -> Intent IR -> NuLang capabilities/agents -> Kokoro -> LiveKit`

The crate intentionally keeps media transport, speech models, and model runtimes behind narrow traits. NuLang remains the semantic and execution boundary.

## Safety model

Partial speech is provisional. It may start only read-only speculative work such as retrieval, repository search, parsing, planning, or cache warming. It must never directly trigger user-visible mutation.

Final speech is converted into a confirmed, modality-neutral `IntentIr`. Only confirmed intent can become an executable `Goal`, and normal capability/policy checks still govern side effects.

## Current adapters

- LiveKit session/media bridge boundary
- Silero VAD runtime seam
- Parakeet streaming STT runtime seam
- Whisper fallback runtime seam
- automatic English/multilingual STT routing
- Kokoro streaming TTS runtime seam
- barge-in aware playback cancellation
- semantic end-of-turn detection
- safe partial-transcript speculation
- voice transcript -> Intent IR bridge
- isolated voice CI for format, check, test, and clippy validation

Concrete model clients remain intentionally outside the provider-neutral core so deployments can choose local inference, GPU workers, sidecars, or hosted providers without forcing those dependencies on every NuLang runtime.

The workspace registration preserves the independently-added `nulang-capacity` crate alongside `nulang-ai-voice`.

Next slices are concrete model-runtime clients, adaptive latency telemetry, and an optional expressive Orpheus TTS tier.

# nulang-ai-voice

Provider-neutral realtime voice adapters for the NuLang agent runtime.

## Implemented

- LiveKit media/session bridge boundary
- Silero VAD adapter seam
- Parakeet streaming STT adapter seam
- Whisper/faster-whisper fallback adapter seam
- automatic STT routing: English realtime -> Parakeet, multilingual/fallback -> Whisper
- Kokoro streaming TTS adapter seam
- barge-in aware playback controller with deterministic TTS cancellation
- semantic end-of-turn detection combining VAD, STT finality, silence, and optional completion scoring
- safe partial-transcript speculation restricted to read-only work, with stale-prefetch cancellation and final-transcript reuse
- isolated voice CI for format, check, test, and clippy validation

Heavy provider runtimes remain outside this crate. Local model runners, GPU workers, Python sidecars, and hosted services implement narrow runtime traits while `nulang-ai-core` owns the semantic voice contract.

The intended data path is:

`LiveKit/WebRTC -> Silero -> Parakeet/Whisper -> turn detection -> speculative read-only prefetch -> Intent IR -> NuLang agent/capabilities -> Kokoro -> LiveKit`

Speculation is deliberately conservative: partial speech may start retrieval, repository search, parsing, planning, or cache warming, but it may not perform externally visible mutations. A compatible final transcript can reuse the prefetched result; an incompatible final transcript cancels it. Side effects still enter through the normal capability-checked final-intent path.

The workspace registration preserves the independently-added `nulang-capacity` crate alongside `nulang-ai-voice`.

Next slices are concrete model-runtime clients, wiring turn/speculation events into the agent runtime, adaptive latency telemetry, and an optional expressive Orpheus TTS tier.

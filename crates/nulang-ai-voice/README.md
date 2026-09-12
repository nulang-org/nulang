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

Heavy provider runtimes remain outside this crate. Local model runners, GPU workers, Python sidecars, and hosted services implement narrow runtime traits while `nulang-ai-core` owns the semantic voice contract.

The intended data path is:

`LiveKit/WebRTC -> Silero -> Parakeet/Whisper -> Intent IR -> NuLang agent/capabilities -> Kokoro -> LiveKit`

Next slices are concrete model-runtime clients, semantic end-of-turn detection, speculative intent/tool prefetch from partial transcripts, and an optional expressive Orpheus TTS tier.

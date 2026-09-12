//! Realtime voice transport and provider adapters for NuLang.
//!
//! This crate intentionally keeps external media/STT/TTS/VAD providers behind
//! narrow interfaces so the agent runtime remains provider-neutral.

pub mod kokoro;
pub mod livekit;
pub mod parakeet;
pub mod playback;
pub mod routing;
pub mod silero;
pub mod whisper;

pub use kokoro::{KokoroRuntime, KokoroRuntimeStream, KokoroSynthesizer};
pub use livekit::{LiveKitEvent, LiveKitSessionBridge, LiveKitSessionConfig};
pub use parakeet::{ParakeetRecognizer, ParakeetRuntime, ParakeetRuntimeSession};
pub use playback::{PlaybackController, PlaybackError};
pub use routing::AutoSpeechRecognizer;
pub use silero::{SileroVadAdapter, SileroVadConfig, SileroVadRuntime};
pub use whisper::{WhisperRecognizer, WhisperRuntime, WhisperRuntimeSession};

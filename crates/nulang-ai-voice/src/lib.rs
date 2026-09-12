//! Realtime voice transport and provider adapters for NuLang.
//!
//! This crate intentionally keeps external media/STT/TTS/VAD providers behind
//! narrow interfaces so the agent runtime remains provider-neutral.

pub mod livekit;
pub mod silero;

pub use livekit::{LiveKitEvent, LiveKitSessionBridge, LiveKitSessionConfig};
pub use silero::{SileroVadAdapter, SileroVadConfig, SileroVadRuntime};

//! AI runtime types for Nulang — LLM clients, memory, pipelines, debates,
//! supervisor teams, and usage tracking.
//!
//! This crate provides the provider-agnostic request/response types, an async
//! LLM client trait, and higher-level orchestration patterns (pipeline, debate,
//! supervisor team). It has no dependency on the core `nulang` crate; the
//! concrete `Runtime` implementations live in the core crate.

pub mod client;
pub mod debate;
pub mod memory;
pub mod mock;
pub mod pipeline;
pub mod procedural_memory;
pub mod providers;
pub mod registry;
pub mod request;
pub mod response;
pub mod semantic_memory;
pub mod supervisor;
pub mod usage;

pub use client::{complete_sync, LlmClient};
pub use debate::{Debate, DebateRuntime, Participant, Stance};
pub use memory::{EpisodicMemory, Turn};
pub use mock::MockLlmClient;
pub use pipeline::{Pipeline, PipelineRuntime, PipelineStage};
pub use procedural_memory::{Pattern, ProceduralMemory};
pub use providers::ollama::OllamaClient;
pub use providers::openai::OpenAiClient;
pub use registry::{AiRuntimeRegistry, SupervisorTeamRegistry};
pub use request::{LlmMessage, LlmRequest, ModelPricing, ToolSchema};
pub use response::{LlmError, LlmErrorKind, LlmResponse, TokenUsage, ToolCall};
pub use semantic_memory::{Document, SemanticMemory};
pub use supervisor::{SupervisorRuntime, SupervisorTeam, Worker};
pub use usage::{estimated_cost, TokenBudget, UsageSummary};

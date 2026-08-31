//! NLAP v1 protocol helpers.

pub use nulang_ai_core::{Goal, GoalGraph, SwarmEvent, SwarmEventEnvelope, Task, NLAP_VERSION};

pub fn format_event_line(envelope: &SwarmEventEnvelope) -> Result<String, serde_json::Error> {
    serde_json::to_string(envelope)
}

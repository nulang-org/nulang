//! Read-only explanation of aggregate allocation eligibility in a bytecode module.
//!
//! This is a static classification, *not* a measurement of runtime allocations.
//! It deliberately calls the same conservative proof used by the opt-in
//! `NULANG_ISO_ARENA` execution path, rather than maintaining a second
//! approximation of the escape rules. The index is the instruction PC in the
//! serialized `.nbc` module (not a source line number).
use crate::bytecode::{CodeModule, OpCode};
use crate::iso_arena::qualifying_alloc_sites;
use serde::Serialize;

/// Analysis status for a bytecode aggregate allocation site.
///
/// `NotProven` is deliberately distinct from `NotAnalyzed`: the former
/// failed the current conservative activation-local proof; neither implies
/// that an actual heap allocation was observed during execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationDisposition {
    ArenaEligible,
    NotProven,
    NotAnalyzed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllocationSite {
    /// Zero-based instruction offset in the bytecode module.
    pub pc: usize,
    pub opcode: OpCode,
    pub disposition: AllocationDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllocationReport {
    /// Version of this reporting schema, independent from the NBC format.
    pub schema_version: u32,
    pub module: String,
    pub total_sites: usize,
    pub arena_eligible: usize,
    pub not_proven: usize,
    pub not_analyzed: usize,
    /// Allocation sites in ascending PC order for reproducible diffs.
    pub sites: Vec<AllocationSite>,
}

/// Explain aggregate allocation candidates without executing or changing the
/// program. Only `ArrAlloc`, `TupleMk`, `RecMk` and `RecCopy` are reported.
/// `RecCopy` is included as `NotAnalyzed` because its retention semantics
/// exclude it from the current iso-arena proof.
///
/// This does not account for strings, allocations inside effects or other
/// runtime helpers, nor does it describe native/AOT or WASM allocation behavior.
pub fn analyze_aggregate_allocations(module: &CodeModule) -> AllocationReport {
    let eligible = qualifying_alloc_sites(module);
    let mut report = AllocationReport {
        schema_version: 1,
        module: module.name.clone(),
        total_sites: 0,
        arena_eligible: 0,
        not_proven: 0,
        not_analyzed: 0,
        sites: Vec::new(),
    };

    for (pc, instruction) in module.instructions.iter().enumerate() {
        let disposition = match instruction.opcode {
            OpCode::ArrAlloc | OpCode::RecMk | OpCode::TupleMk => {
                if eligible.contains(&pc) {
                    AllocationDisposition::ArenaEligible
                } else {
                    AllocationDisposition::NotProven
                }
            }
            OpCode::RecCopy => AllocationDisposition::NotAnalyzed,
            _ => continue,
        };

        match disposition {
            AllocationDisposition::ArenaEligible => report.arena_eligible += 1,
            AllocationDisposition::NotProven => report.not_proven += 1,
            AllocationDisposition::NotAnalyzed => report.not_analyzed += 1,
        }
        report.sites.push(AllocationSite {
            pc,
            opcode: instruction.opcode,
            disposition,
        });
    }
    report.total_sites = report.sites.len();
    report
}

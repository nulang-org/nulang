//! Differential audit of *prospective* incremental compiler results.
//!
//! This is a diagnostic oracle, not an incremental compiler, cache key, or
//! permission to skip checking/lowering. The caller must obtain both clean
//! observations from independent complete compiler runs; a matching report is
//! only as trustworthy as those observations and the input closure supplied to
//! `ModuleSnapshot`. No production execution path calls this module.

use std::collections::{BTreeMap, BTreeSet};

use super::module_invalidation::{invalidated_modules, Invalidation, ModuleSnapshot};

/// Digests of compiler-owned outputs, not of guessed source-level summaries.
///
/// Absent later-stage outputs represent a build that did not reach that stage.
/// Diagnostics must include the complete ordered diagnostics for the module,
/// including errors and warnings. The caller must define deterministic encoding
/// for every stage before using this in a compiler harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerOutput {
    pub diagnostics: [u8; 32],
    pub typed_facts: Option<[u8; 32]>,
    pub mir: Option<[u8; 32]>,
    pub artifact: Option<[u8; 32]>,
}

impl CompilerOutput {
    /// Hash actual compiler-emitted bytes. No content is inferred or repaired.
    pub fn from_bytes(
        diagnostics: &[u8],
        typed_facts: Option<&[u8]>,
        mir: Option<&[u8]>,
        artifact: Option<&[u8]>,
    ) -> Self {
        fn digest(bytes: &[u8]) -> [u8; 32] {
            *blake3::hash(bytes).as_bytes()
        }
        Self {
            diagnostics: digest(diagnostics),
            typed_facts: typed_facts.map(digest),
            mir: mir.map(digest),
            artifact: artifact.map(digest),
        }
    }
}

/// Outputs of a complete (clean, or proposed incremental) build.
///
/// `context_digest` and module names are checked against the claimed input
/// snapshot, but this type cannot establish that all compiler inputs were
/// included. That guarantee must come from compiler-owned collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildObservation {
    pub context_digest: [u8; 32],
    pub modules: BTreeMap<String, CompilerOutput>,
}

impl BuildObservation {
    pub fn new(snapshot: &ModuleSnapshot) -> Self {
        Self {
            context_digest: snapshot.context_digest,
            modules: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, name: impl Into<String>, output: CompilerOutput) {
        self.modules.insert(name.into(), output);
    }
}

/// A consistency report is not a certificate authorizing compiled-code reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleVerdict {
    /// No contradiction found within the supplied before/after observations.
    Consistent {
        invalidated: Vec<String>,
        reusable: Vec<String>,
        /// Report only: equal outputs are insufficient to prove that an
        /// invalidation edge is unnecessary for arbitrary future edits.
        potentially_redundant: Vec<String>,
        /// False means no proposed incremental output was compared at all.
        candidate_checked: bool,
    },
    /// The supplied observations contradict the proposed frontier or candidate.
    Rejected { reason: String },
    /// Missing/invalid dependency closure or compiler changes forbid reuse.
    FullRebuildRequired { reason: String },
}

/// Audit an incremental *plan* against independently compiled before/after
/// results, and optionally compare a proposed incremental result to clean-after.
///
/// This function never writes, reads, or returns a cache entry. It rejects
/// incomplete or mismatched observation sets, a changed output of any module
/// the plan proposes to reuse, and any stale output in a proposed candidate.
/// Clean observations are assumed to come from actual compiler invocations;
/// this function cannot authenticate their provenance or prove completeness.
pub fn audit_incremental_plan(
    before: &ModuleSnapshot,
    after: &ModuleSnapshot,
    clean_before: &BuildObservation,
    clean_after: &BuildObservation,
    candidate_after: Option<&BuildObservation>,
) -> OracleVerdict {
    let invalidated = match invalidated_modules(before, after) {
        Invalidation::FullRebuild { reason } => {
            return OracleVerdict::FullRebuildRequired { reason };
        }
        Invalidation::Dirty { modules } => modules,
        Invalidation::Unchanged => Vec::new(),
    };

    for (label, snapshot, observed) in [
        ("clean-before", before, clean_before),
        ("clean-after", after, clean_after),
    ] {
        if let Err(reason) = validate_observation(label, snapshot, observed) {
            return OracleVerdict::Rejected { reason };
        }
    }
    if let Some(candidate) = candidate_after {
        if let Err(reason) = validate_observation("candidate", after, candidate) {
            return OracleVerdict::Rejected { reason };
        }
    }

    let dirty: BTreeSet<&str> = invalidated.iter().map(String::as_str).collect();
    let mut reusable = Vec::new();
    for (name, new_output) in &clean_after.modules {
        if dirty.contains(name.as_str()) {
            continue;
        }
        // Any unchanged module in a valid plan must exist in the old build.
        let Some(old_output) = clean_before.modules.get(name) else {
            return OracleVerdict::Rejected {
                reason: format!("unchanged module '{name}' has no clean-before observation"),
            };
        };
        let changed = changed_stages(old_output, new_output);
        if !changed.is_empty() {
            return OracleVerdict::Rejected {
                reason: format!(
                    "under-invalidation: module '{name}' was marked reusable but clean outputs differ at {}",
                    changed.join(", ")
                ),
            };
        }
        reusable.push(name.clone());
    }

    if let Some(candidate) = candidate_after {
        for (name, expected) in &clean_after.modules {
            let actual = candidate
                .modules
                .get(name)
                .expect("candidate module coverage already validated");
            let changed = changed_stages(expected, actual);
            if !changed.is_empty() {
                return OracleVerdict::Rejected {
                    reason: format!(
                        "candidate output for module '{name}' differs from clean build at {}",
                        changed.join(", ")
                    ),
                };
            }
        }
    }

    let potentially_redundant = invalidated
        .iter()
        .filter(|name| clean_before.modules.get(*name) == clean_after.modules.get(*name))
        .cloned()
        .collect();

    OracleVerdict::Consistent {
        invalidated,
        reusable,
        potentially_redundant,
        candidate_checked: candidate_after.is_some(),
    }
}

fn validate_observation(
    label: &str,
    snapshot: &ModuleSnapshot,
    observation: &BuildObservation,
) -> Result<(), String> {
    if observation.context_digest != snapshot.context_digest {
        return Err(format!(
            "{label} context digest does not match input snapshot"
        ));
    }

    let expected: BTreeSet<&String> = snapshot.modules.keys().collect();
    let actual: BTreeSet<&String> = observation.modules.keys().collect();
    if expected != actual {
        let missing: Vec<_> = expected
            .difference(&actual)
            .map(|name| name.as_str())
            .collect();
        let extra: Vec<_> = actual
            .difference(&expected)
            .map(|name| name.as_str())
            .collect();
        return Err(format!(
            "{label} module observations incomplete or unexpected: missing=[{}], extra=[{}]",
            missing.join(", "),
            extra.join(", ")
        ));
    }

    for (name, output) in &observation.modules {
        if output.artifact.is_some() && output.mir.is_none() {
            return Err(format!(
                "{label} module '{name}' has an artifact without MIR"
            ));
        }
        if output.mir.is_some() && output.typed_facts.is_none() {
            return Err(format!(
                "{label} module '{name}' has MIR without type/effect facts"
            ));
        }
    }
    Ok(())
}

fn changed_stages(old: &CompilerOutput, new: &CompilerOutput) -> Vec<&'static str> {
    let mut changed = Vec::new();
    if old.diagnostics != new.diagnostics {
        changed.push("diagnostics");
    }
    if old.typed_facts != new.typed_facts {
        changed.push("typed_facts");
    }
    if old.mir != new.mir {
        changed.push("mir");
    }
    if old.artifact != new.artifact {
        changed.push("artifact");
    }
    changed
}

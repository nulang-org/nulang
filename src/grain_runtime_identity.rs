//! Runtime-facing grain identity activation helpers.
//!
//! These methods keep the large actor runtime call sites small: activation first
//! performs a pure identity preflight, does recovery/hydration work, then commits
//! all resident indexes only after activation succeeds.

use crate::runtime::{
    bind_resident_grain_indexes, grain_actor_id, validate_resident_grain_indexes, GrainId, Runtime,
};
use crate::types::NuError;

impl Runtime {
    /// Compute the v1 compact id and validate every in-memory identity relation
    /// without mutating runtime state.
    ///
    /// `resolve_or_hydrate_grain` should call this before recovery-module
    /// registration, snapshot access, actor insertion, or replay.
    pub(crate) fn preflight_grain_activation_identity(
        &self,
        grain_id: &GrainId,
    ) -> Result<u64, NuError> {
        let actor_id = grain_actor_id(grain_id);
        validate_resident_grain_indexes(
            &self.grain_residents,
            &self.actor_grain_id,
            &self.grain_actor_ids,
            actor_id,
            grain_id,
        )?;
        Ok(actor_id)
    }

    /// Commit the complete resident identity relation after successful actor
    /// activation. Validation is repeated inside the binding primitive so a
    /// changed relation cannot be partially committed.
    pub(crate) fn commit_grain_activation_identity(
        &mut self,
        actor_id: u64,
        grain_id: GrainId,
    ) -> Result<(), NuError> {
        bind_resident_grain_indexes(
            &mut self.grain_residents,
            &mut self.actor_grain_id,
            &mut self.grain_actor_ids,
            actor_id,
            grain_id,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_preflight_is_pure_and_returns_stable_id() {
        let runtime = Runtime::new();
        let grain = GrainId::new("User", "42");
        let expected = grain_actor_id(&grain);

        assert_eq!(
            runtime.preflight_grain_activation_identity(&grain).unwrap(),
            expected
        );
        assert!(runtime.grain_residents.is_empty());
        assert!(runtime.actor_grain_id.is_empty());
        assert!(runtime.grain_actor_ids.is_empty());
    }

    #[test]
    fn runtime_preflight_rejects_compact_id_collision() {
        let mut runtime = Runtime::new();
        let requested = GrainId::new("User", "42");
        let actor_id = grain_actor_id(&requested);
        runtime
            .grain_actor_ids
            .insert(actor_id, GrainId::new("Other", "logical-id"));

        let error = runtime
            .preflight_grain_activation_identity(&requested)
            .unwrap_err();
        assert!(error.to_string().contains("grain actor-id collision"));
        assert_eq!(runtime.grain_actor_ids.len(), 1);
        assert!(runtime.grain_residents.is_empty());
    }

    #[test]
    fn runtime_commit_updates_all_identity_indexes() {
        let mut runtime = Runtime::new();
        let grain = GrainId::new("User", "42");
        let actor_id = runtime
            .preflight_grain_activation_identity(&grain)
            .unwrap();

        runtime
            .commit_grain_activation_identity(actor_id, grain.clone())
            .unwrap();

        assert_eq!(runtime.grain_residents.get(&grain), Some(&actor_id));
        assert_eq!(runtime.actor_grain_id.get(&actor_id), Some(&grain));
        assert_eq!(runtime.grain_actor_ids.get(&actor_id), Some(&grain));
    }

    #[test]
    fn runtime_preflight_rejects_logical_rebind_after_dehydration() {
        let mut runtime = Runtime::new();
        let grain = GrainId::new("User", "42");
        runtime.actor_grain_id.insert(7, grain.clone());
        runtime.grain_actor_ids.insert(7, grain.clone());
        assert!(runtime.grain_residents.is_empty());

        // Use a grain whose deterministic id is not the synthetic old id 7.
        assert_ne!(grain_actor_id(&grain), 7);
        let error = runtime
            .preflight_grain_activation_identity(&grain)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("grain logical-identity conflict"));
    }
}

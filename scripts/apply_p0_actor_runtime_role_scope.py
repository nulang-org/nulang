#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


path = Path("src/runtime/mod.rs")
text = path.read_text()

text = replace_once(
    text,
    '''        let workflow_events = self.persistence.read_workflow_events(actor_id);
        let is_workflow = self
            .recovery_modules
            .get(&actor_id)
            .map(|(m, _, _)| m.actor_metadata.iter().any(|meta| meta.is_workflow))
            .unwrap_or(!workflow_events.is_empty());
        let is_agent = self
            .recovery_modules
            .get(&actor_id)
            .map(|(m, _, _)| m.actor_metadata.iter().any(|meta| meta.is_agent))
            .unwrap_or(false);''',
    '''        let workflow_events = self.persistence.read_workflow_events(actor_id);
        let recovery_meta = self
            .recovery_modules
            .get(&actor_id)
            .and_then(|(module, _, _)| {
                Self::module_meta_for_schema(module, snapshot.bytecode_schema_name.as_deref())
            });
        let is_workflow = recovery_meta
            .map(|meta| meta.is_workflow)
            .unwrap_or(!workflow_events.is_empty());
        let is_agent = recovery_meta.map(|meta| meta.is_agent).unwrap_or(false);''',
    "recovery role scope",
)

text = replace_once(
    text,
    '''            actor.state_models = module
                .actor_metadata
                .iter()
                .flat_map(|m| &m.state_models)
                .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
                .collect();''',
    '''            actor.state_models = Self::module_meta_for_schema(
                module,
                actor.bytecode_schema_name.as_deref(),
            )
            .into_iter()
            .flat_map(|meta| &meta.state_models)
            .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
            .collect();''',
    "recovery state-model scope",
)

text = replace_once(
    text,
    '''        actor.state_models = module
            .actor_metadata
            .iter()
            .flat_map(|m| &m.state_models)
            .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
            .collect();''',
    '''        actor.state_models = Self::module_meta_for_schema(
            module,
            actor.bytecode_schema_name.as_deref(),
        )
        .into_iter()
        .flat_map(|meta| &meta.state_models)
        .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
        .collect();''',
    "snapshot restore state-model scope",
)

text = replace_once(
    text,
    '''        for (name, c) in module.actor_metadata.iter().flat_map(|m| &m.state_defaults) {
            if actor.get_state_field(name).is_some() {''',
    '''        for (name, c) in Self::module_meta_for_schema(
            module,
            actor.bytecode_schema_name.as_deref(),
        )
        .into_iter()
        .flat_map(|meta| &meta.state_defaults)
        {
            if actor.get_state_field(name).is_some() {''',
    "snapshot restore defaults scope",
)

text = replace_once(
    text,
    '''        let actor = if let Some(ref snap) = snapshot {''',
    '''        let mut actor = if let Some(ref snap) = snapshot {''',
    "grain actor mutable",
)
text = replace_once(
    text,
    '''        // Track the grain identity.
        self.actors.insert(stable_actor_id, actor);''',
    '''        // The stable grain type is the nominal ActorMeta owner even for
        // legacy snapshots that predate persisted schema identity.
        actor.bytecode_schema_name = Some(grain_id.grain_type.clone());

        // Track the grain identity.
        self.actors.insert(stable_actor_id, actor);''',
    "grain schema restore",
)

text = replace_once(
    text,
    '''        let is_workflow = module.actor_metadata.iter().any(|m| m.is_workflow);
        let is_agent = module.actor_metadata.iter().any(|m| m.is_agent);''',
    '''        let migrated_meta = Self::module_meta_for_schema(
            &module,
            snapshot.bytecode_schema_name.as_deref(),
        );
        let is_workflow = migrated_meta.map(|meta| meta.is_workflow).unwrap_or(false);
        let is_agent = migrated_meta.map(|meta| meta.is_agent).unwrap_or(false);''',
    "migration role scope",
)

path.write_text(text)

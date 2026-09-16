#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def replace_n(text: str, old: str, new: str, count: int, label: str) -> str:
    actual = text.count(old)
    if actual != count:
        raise SystemExit(f"{label}: expected exactly {count} matches, found {actual}")
    return text.replace(old, new)


def main() -> None:
    path = Path("src/mir_lower.rs")
    text = path.read_text()

    text = replace_once(
        text,
        '''    /// Resolve send/ask only from the receiver's nominal actor owner.
    /// Global suffix fallback is forbidden because different actors may expose
    /// the same short behavior name.
    fn send_behavior_idx(&self, actor_name_hint: &str, behavior: &str) -> Option<usize> {
        if actor_name_hint.is_empty() {
            return None;
        }
        let full_name = format!("{}.{}", actor_name_hint, behavior);
        self.behavior_names.iter().position(|name| *name == full_name)
    }
''',
        '''    /// Resolve send/ask from receiver provenance when available. If the
    /// receiver is intentionally dynamic, preserve legacy programs only when
    /// the short behavior name is globally unambiguous; ambiguous names fail
    /// closed instead of binding to declaration order.
    fn send_behavior_idx(&self, actor_owner: Option<&str>, behavior: &str) -> Option<usize> {
        if let Some(owner) = actor_owner {
            let full_name = format!("{}.{}", owner, behavior);
            return self.behavior_names.iter().position(|name| *name == full_name);
        }

        let suffix = format!(".{}", behavior);
        let mut matches = self
            .behavior_names
            .iter()
            .enumerate()
            .filter_map(|(idx, name)| name.ends_with(&suffix).then_some(idx));
        let first = matches.next()?;
        if matches.next().is_some() {
            None
        } else {
            Some(first)
        }
    }
''',
        "behavior resolver",
    )

    text = replace_once(
        text,
        '''    /// Stack of active handler scopes.  Each entry is `(table_index, [(binding_index, effect_qualified_name)])`.
    /// Pushed when lowering a `handle` block (after `EnterHandle`) and
    /// popped when the `handle`'s join block is reached.  Used to resolve
    /// `Perform` operations to a statically-known `HandlerRef`.
    handler_scope: Vec<(usize, Vec<(usize, String)>)>,
}''',
        '''    /// Stack of active handler scopes.  Each entry is `(table_index, [(binding_index, effect_qualified_name)])`.
    /// Pushed when lowering a `handle` block (after `EnterHandle`) and
    /// popped when the `handle`'s join block is reached.  Used to resolve
    /// `Perform` operations to a statically-known `HandlerRef`.
    handler_scope: Vec<(usize, Vec<(usize, String)>)>,
    /// Compile-time actor declaration provenance for MIR locals. Runtime actor
    /// references remain unchanged; this map exists only to make behavior
    /// dispatch receiver-specific after HIR's intentionally lossy type erasure.
    actor_owners: FxHashMap<mir::LocalId, String>,
}''',
        "FnLowerer provenance field",
    )

    text = replace_once(
        text,
        '''            handle_depth: 0,
            handler_scope: Vec::new(),
        }''',
        '''            handle_depth: 0,
            handler_scope: Vec::new(),
            actor_owners: FxHashMap::default(),
        }''',
        "FnLowerer provenance init",
    )

    text = replace_once(
        text,
        '''    fn lookup(&self, name: &str) -> Option<mir::LocalId> {
        for scope in self.scopes.iter().rev() {
            for (n, id) in scope.iter().rev() {
                if n == name {
                    return Some(*id);
                }
            }
        }
        None
    }

    // -- Body lowering ------------------------------------------------------''',
        '''    fn lookup(&self, name: &str) -> Option<mir::LocalId> {
        for scope in self.scopes.iter().rev() {
            for (n, id) in scope.iter().rev() {
                if n == name {
                    return Some(*id);
                }
            }
        }
        None
    }

    fn seed_actor_owner_from_type(&mut self, id: mir::LocalId, ty: &Type) {
        if let Some(owner) = nominal_actor_name_from_type(ty) {
            self.actor_owners.insert(id, owner);
        }
    }

    // -- Body lowering ------------------------------------------------------''',
        "provenance helper",
    )

    text = replace_once(
        text,
        '''    for (name, ty) in &f.params {
        let id = lowerer.b.add_param(name.clone(), ty.clone());
        lowerer.bind(name, id);
    }''',
        '''    for (name, ty) in &f.params {
        let id = lowerer.b.add_param(name.clone(), ty.clone());
        lowerer.bind(name, id);
        lowerer.seed_actor_owner_from_type(id, ty);
    }''',
        "function parameter provenance",
    )

    text = replace_once(
        text,
        '''    for (name, ty) in &bh.params {
        let id = lowerer.b.add_param(name.clone(), ty.clone());
        lowerer.bind(name, id);
    }
    let self_id = lowerer.b.add_local("self", Type::unit());
    lowerer.b.assign(self_id, mir::RValue::SelfRef);
    lowerer.bind("self", self_id);''',
        '''    for (name, ty) in &bh.params {
        let id = lowerer.b.add_param(name.clone(), ty.clone());
        lowerer.bind(name, id);
        lowerer.seed_actor_owner_from_type(id, ty);
    }
    let self_id = lowerer.b.add_local("self", Type::unit());
    lowerer.b.assign(self_id, mir::RValue::SelfRef);
    lowerer.bind("self", self_id);
    if let Some((owner, _)) = full_name.rsplit_once('.') {
        lowerer.actor_owners.insert(self_id, owner.to_string());
    }''',
        "behavior parameter/self provenance",
    )

    text = replace_once(
        text,
        '''    fn lower_rvalue(&mut self, dst: mir::LocalId, rv: &hir::RValue) -> NuResult<()> {
        use crate::bytecode::Constant;
        match rv {''',
        '''    fn lower_rvalue(&mut self, dst: mir::LocalId, rv: &hir::RValue) -> NuResult<()> {
        use crate::bytecode::Constant;
        // Every assignment replaces the destination's previous provenance.
        // Individual actor-preserving rvalues below re-establish it.
        self.actor_owners.remove(&dst);
        match rv {''',
        "clear overwritten provenance",
    )

    text = replace_once(
        text,
        '''            hir::RValue::Use(op) => {
                let id = self.lower_operand(op)?;
                self.b.assign(dst, mir::RValue::Load(id));
                Ok(())
            }''',
        '''            hir::RValue::Use(op) => {
                let id = self.lower_operand(op)?;
                let actor_owner = self.actor_owners.get(&id).cloned();
                self.b.assign(dst, mir::RValue::Load(id));
                if let Some(owner) = actor_owner {
                    self.actor_owners.insert(dst, owner);
                }
                Ok(())
            }''',
        "alias provenance",
    )

    text = replace_once(
        text,
        '''                self.b.assign(
                    dst,
                    mir::RValue::Spawn {
                        behavior_idx: idx,
                        init: init_rvs,
                        target_node: target_local,
                        capabilities: capabilities.clone(),
                    },
                );
                Ok(())
            }''',
        '''                self.b.assign(
                    dst,
                    mir::RValue::Spawn {
                        behavior_idx: idx,
                        init: init_rvs,
                        target_node: target_local,
                        capabilities: capabilities.clone(),
                    },
                );
                self.actor_owners.insert(dst, actor_type.clone());
                Ok(())
            }''',
        "spawn provenance",
    )

    old_dispatch = '''                let actor_hint = operand_name_hint(actor);
                let idx = self
                    .ctx
                    .send_behavior_idx(&actor_hint, behavior)
                    .ok_or_else(|| {
                        compile_err(
                            format!(
                                "cannot resolve behavior '{}.{}' from receiver type",
                                actor_hint, behavior
                            ),
                            Span::default(),
                        )
                    })?;
                let actor_id = self.lower_operand(actor)?;'''
    new_dispatch = '''                let actor_id = self.lower_operand(actor)?;
                let actor_owner = self.actor_owners.get(&actor_id).cloned();
                let idx = self
                    .ctx
                    .send_behavior_idx(actor_owner.as_deref(), behavior)
                    .ok_or_else(|| {
                        compile_err(
                            match actor_owner {
                                Some(ref owner) => format!(
                                    "actor '{}' does not declare behavior '{}'",
                                    owner, behavior
                                ),
                                None => format!(
                                    "cannot resolve behavior '{}' unambiguously from dynamic receiver",
                                    behavior
                                ),
                            },
                            Span::default(),
                        )
                    })?;'''
    text = replace_n(text, old_dispatch, new_dispatch, 2, "send/ask provenance dispatch")

    path.write_text(text)


if __name__ == "__main__":
    main()

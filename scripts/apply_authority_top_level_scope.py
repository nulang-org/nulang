#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {count}")
    return text.replace(old, new, 1)

path = Path("src/runtime/callbacks.rs")
text = path.read_text()

# Actor IDs are allocated from 1. AOT top-level execution intentionally uses
# BytecodeRuntimeCallbacks with actor_id=0 as its actor-free sentinel. Keep
# that historic top-level contract while continuing to fail closed for every
# real (nonzero) actor identity.
text = replace_once(
    text,
    '''impl BytecodeRuntimeCallbacks {\n    pub(crate) fn new(runtime: *mut Runtime, actor_id: u64) -> Self {\n        BytecodeRuntimeCallbacks { runtime, actor_id }\n    }\n}\n''',
    '''impl BytecodeRuntimeCallbacks {\n    pub(crate) fn new(runtime: *mut Runtime, actor_id: u64) -> Self {\n        BytecodeRuntimeCallbacks { runtime, actor_id }\n    }\n\n    fn authority_actor_id(&self) -> Option<u64> {\n        (self.actor_id != 0).then_some(self.actor_id)\n    }\n}\n''',
    "BytecodeRuntimeCallbacks authority subject helper",
)

text = replace_once(
    text,
    '''        unsafe { authorize_actor_ffi(&*self.runtime, Some(self.actor_id), library, symbol).is_ok() }''',
    '''        unsafe {\n            authorize_actor_ffi(&*self.runtime, self.authority_actor_id(), library, symbol).is_ok()\n        }''',
    "bytecode FFI top-level sentinel",
)

text = replace_once(
    text,
    '''                Some(self.actor_id),\n                effect_name,\n                op_name,\n                &module.constants,''',
    '''                self.authority_actor_id(),\n                effect_name,\n                op_name,\n                &module.constants,''',
    "bytecode host-effect top-level sentinel",
)

text = replace_once(
    text,
    '''                Some(self.actor_id),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],''',
    '''                self.authority_actor_id(),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],''',
    "bytecode sync LLM top-level sentinel",
)

text = replace_once(
    text,
    '''                Some(actor_id),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],''',
    '''                (actor_id != 0).then_some(actor_id),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],''',
    "bytecode async LLM top-level sentinel",
)

needle = '''    #[test]\n    fn actor_free_runtime_keeps_existing_ambient_host_contract() {'''
test = '''    #[test]\n    fn bytecode_top_level_zero_sentinel_is_actor_free_but_missing_real_actor_fails_closed() {\n        let mut rt = Runtime::new();\n        let top_level = super::BytecodeRuntimeCallbacks::new(&mut rt as *mut Runtime, 0);\n        assert_eq!(top_level.authority_actor_id(), None);\n\n        let missing_actor = super::BytecodeRuntimeCallbacks::new(&mut rt as *mut Runtime, 999_999);\n        assert_eq!(missing_actor.authority_actor_id(), Some(999_999));\n\n        let (constants, regs) = string_args(&["/tmp/sentinel.txt"]);\n        assert!(authorize_actor_host_effect(\n            &rt,\n            top_level.authority_actor_id(),\n            "FS",\n            Some("read"),\n            &constants,\n            &regs,\n        )\n        .is_ok());\n        assert!(authorize_actor_host_effect(\n            &rt,\n            missing_actor.authority_actor_id(),\n            "FS",\n            Some("read"),\n            &constants,\n            &regs,\n        )\n        .is_err());\n    }\n\n'''
if text.count(needle) != 1:
    raise SystemExit(f"top-level scope test anchor drift: {text.count(needle)}")
text = text.replace(needle, test + needle, 1)

path.write_text(text)
print("top-level actor authority scope patch applied")

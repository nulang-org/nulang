from pathlib import Path

path = Path("src/runtime/callbacks.rs")
text = path.read_text()

replacements = [
    (
        '''        ("Provider", Some("ask")) => other("Provider", "Ask", Some(string_arg(0)?)),\n        ("Inference" | "LLM", Some("ask")) => other("Inference", "Ask", None),\n''',
        '''''',
    ),
    (
        '''        if authorize_actor_host_effect(\n            &rt,\n            rt.current_actor,\n            "Inference",\n            Some("ask"),\n            &[],\n            &[],\n        )\n        .is_err()\n        {\n            return None;\n        }\n''',
        '''''',
    ),
    (
        '''            if authorize_actor_host_effect(\n                rt,\n                self.authority_actor_id(),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],\n            )\n            .is_err()\n            {\n                return None;\n            }\n''',
        '''''',
    ),
    (
        '''            // Authority must still be valid immediately before the first\n            // externally-observable provider request. Completed/resumed calls\n            // above do not re-authorize an operation that already crossed the\n            // boundary.\n            if authorize_actor_host_effect(\n                rt,\n                (actor_id != 0).then_some(actor_id),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],\n            )\n            .is_err()\n            {\n                return PerformAsyncResult::Ready(None);\n            }\n\n''',
        '''''',
    ),
]

for old, new in replacements:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one exact replacement, found {count}: {old[:80]!r}")
    text = text.replace(old, new, 1)

anchor = '''    #[test]\n    fn web_static_and_realtime_broadcast_map_to_exact_authority() {\n'''
if text.count(anchor) != 1:
    raise SystemExit("test anchor missing or ambiguous")
regression = '''    #[test]\n    fn logical_ai_services_are_not_host_resource_boundaries() {\n        let (constants, regs) = string_args(&["llm", "hello"]);\n        assert_eq!(\n            required_host_authority("Provider", Some("ask"), &constants, &regs).unwrap(),\n            None\n        );\n        assert_eq!(\n            required_host_authority("Inference", Some("ask"), &[], &[]).unwrap(),\n            None\n        );\n        assert_eq!(\n            required_host_authority("LLM", Some("ask"), &[], &[]).unwrap(),\n            None\n        );\n    }\n\n'''
text = text.replace(anchor, regression + anchor, 1)

for forbidden in [
    '("Provider", Some("ask")) => other("Provider", "Ask"',
    '("Inference" | "LLM", Some("ask")) => other("Inference", "Ask"',
]:
    if forbidden in text:
        raise SystemExit(f"logical AI authority gate remains: {forbidden}")

if 'fn logical_ai_services_are_not_host_resource_boundaries()' not in text:
    raise SystemExit("AI boundary regression test was not installed")

path.write_text(text)
print("AI runtime authority boundary correction applied")

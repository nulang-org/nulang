#!/usr/bin/env python3
from pathlib import Path

path = Path("src/runtime/callbacks.rs")
text = path.read_text()
needle = '''    #[test]\n    fn actor_free_runtime_keeps_existing_ambient_host_contract() {'''
if text.count(needle) != 1:
    raise SystemExit(f"host integration test anchor drift: {text.count(needle)}")

test = '''    #[test]\n    fn runtime_callback_denies_http_serve_before_socket_bind() {\n        use crate::vm::ActorVmCallbacks;\n        use std::cell::RefCell;\n        use std::rc::Rc;\n\n        let runtime = Rc::new(RefCell::new(Runtime::new()));\n        let actor_id = 800_005;\n        {\n            let mut rt = runtime.borrow_mut();\n            rt.actors\n                .insert(actor_id, Actor::new(actor_id, "host-http-denied", 8));\n            rt.current_actor = Some(actor_id);\n        }\n\n        let mut callbacks = super::RuntimeVmCallbacks::new(runtime.clone());\n        let module = crate::bytecode::CodeModule::new("host-http-denied");\n        let result = callbacks.perform_builtin_effect_in_module(\n            "Http",\n            Some("serve"),\n            &module,\n            &[Value::int(0), Value::int(0)],\n        );\n\n        assert!(result.expect("denied host effect must be handled").is_nil());\n        assert!(\n            runtime.borrow().http_server.is_none(),\n            "denied Http.serve must not bind a socket"\n        );\n    }\n\n'''
path.write_text(text.replace(needle, test + needle, 1))
print("host authority execution regression added")

#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {count}")
    return text.replace(old, new, 1)

path = Path("src/runtime/callbacks.rs")
text = path.read_text()

# Web.serve_static reaches std::fs directly inside perform_web_builtin, so it
# must require the same exact Fs::Read(path) grant as FS.read. Realtime
# broadcast is externally observable output and gets an exact room-scoped
# capability rather than a broad network permission.
text = replace_once(
    text,
    '''        ("FS", Some("write" | "append")) => AuthorityGrant::FsWrite {\n            path: string_arg(0)?,\n        },\n        ("Env", Some("get")) => AuthorityGrant::EnvRead {''',
    '''        ("FS", Some("write" | "append")) => AuthorityGrant::FsWrite {\n            path: string_arg(0)?,\n        },\n        ("Web", Some("serve_static")) => AuthorityGrant::FsRead {\n            path: string_arg(0)?,\n        },\n        ("Realtime", Some("broadcast")) => {\n            other("Realtime", "Broadcast", Some(string_arg(0)?))\n        },\n        ("Env", Some("get")) => AuthorityGrant::EnvRead {''',
    "Web/Realtime authority mapping",
)

needle = '''    #[test]\n    fn actor_host_access_is_deny_by_default_and_exact_match_only() {'''
extra_tests = r'''    #[test]
    fn web_static_and_realtime_broadcast_map_to_exact_authority() {
        let (constants, regs) = string_args(&["/srv/public/index.html"]);
        assert_eq!(
            required_host_authority("Web", Some("serve_static"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::FsRead {
                path: "/srv/public/index.html".into(),
            })
        );

        let (constants, regs) = string_args(&["orders", "changed"]);
        assert_eq!(
            required_host_authority("Realtime", Some("broadcast"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::Other {
                namespace: "Realtime".into(),
                operation: "Broadcast".into(),
                argument: Some("orders".into()),
            })
        );
    }

    #[test]
    fn runtime_callback_blocks_web_serve_static_without_exact_file_grant() {
        use crate::vm::ActorVmCallbacks;
        use std::cell::RefCell;
        use std::rc::Rc;

        let unique = format!(
            "nulang-authority-static-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let file = std::env::temp_dir().join(unique);
        std::fs::write(&file, "authority-protected-static-content").unwrap();
        let file_string = file.to_string_lossy().into_owned();

        let runtime = Rc::new(RefCell::new(Runtime::new()));
        let actor_id = 800_006;
        {
            let mut rt = runtime.borrow_mut();
            rt.actors
                .insert(actor_id, Actor::new(actor_id, "web-static-authority", 8));
            rt.current_actor = Some(actor_id);
        }

        let module = {
            let mut module = crate::bytecode::CodeModule::new("web-static-authority");
            module.add_constant(Constant::String(file_string.clone()));
            module
        };
        let regs = [Value::string(0)];
        let mut callbacks = super::RuntimeVmCallbacks::new(runtime.clone());

        let denied = callbacks
            .perform_builtin_effect_in_module("Web", Some("serve_static"), &module, &regs)
            .expect("denied Web.serve_static must be handled");
        assert!(denied.is_nil(), "unprivileged actor must not read static file");

        {
            let mut rt = runtime.borrow_mut();
            let token = format!("Fs::Read({file_string})");
            let manifest = AuthorityManifest::from_tokens([token.as_str()]).unwrap();
            rt.actors
                .get_mut(&actor_id)
                .unwrap()
                .install_authority_manifest(&manifest);
        }

        let allowed = callbacks
            .perform_builtin_effect_in_module("Web", Some("serve_static"), &module, &regs)
            .expect("authorized Web.serve_static must be handled");
        assert_eq!(
            crate::vm::resolve_value_string(&[], allowed),
            "authority-protected-static-content"
        );

        let _ = std::fs::remove_file(file);
    }

'''
if text.count(needle) != 1:
    raise SystemExit(f"Web/Realtime test anchor drift: {text.count(needle)}")
text = text.replace(needle, extra_tests + needle, 1)

path.write_text(text)
print("Web/Realtime authority bypass patch applied")

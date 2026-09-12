#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {count}")
    return text.replace(old, new, 1)


path = Path("src/runtime/callbacks.rs")
text = path.read_text()

anchor = '''/// Shared Web effect host implementation for all runtime callback types.\n/// Mirrors the standalone VM dispatch in `src/vm.rs`.\npub(crate) fn perform_web_builtin('''
helper = r'''/// Resolve the exact typed authority required before an actor-backed host
/// effect may cross the runtime boundary. `None` means the effect is not an
/// external-authority operation handled by this gate. Invalid resource
/// descriptions are errors rather than permissive fallbacks.
fn required_host_authority(
    effect_name: &str,
    op_name: Option<&str>,
    constants: &[crate::bytecode::Constant],
    regs: &[crate::vm::Value],
) -> Result<Option<crate::authority::AuthorityGrant>, String> {
    use crate::authority::AuthorityGrant;

    let string_arg = |idx: usize| -> Result<String, String> {
        let value = regs
            .get(idx)
            .copied()
            .ok_or_else(|| format!("missing host-effect argument {idx}"))?;
        let value = crate::vm::resolve_value_string(constants, value);
        if value.is_empty() {
            return Err(format!("host-effect argument {idx} must be non-empty"));
        }
        Ok(value)
    };
    let other = |namespace: &str, operation: &str, argument: Option<String>| {
        AuthorityGrant::Other {
            namespace: namespace.to_string(),
            operation: operation.to_string(),
            argument,
        }
    };

    let grant = match (effect_name, op_name) {
        ("FS", Some("read" | "exists")) => AuthorityGrant::FsRead {
            path: string_arg(0)?,
        },
        ("FS", Some("write" | "append")) => AuthorityGrant::FsWrite {
            path: string_arg(0)?,
        },
        ("Env", Some("get")) => AuthorityGrant::EnvRead {
            name: string_arg(0)?,
        },
        ("Secret", Some("read" | "get")) => AuthorityGrant::SecretRead {
            name: string_arg(0)?,
        },
        ("Http", Some("get" | "post")) => http_outbound_authority(&string_arg(0)?)?,
        ("Http", Some("serve")) => {
            let port = regs
                .first()
                .and_then(|v| v.as_int())
                .ok_or_else(|| "Http.serve requires an integer port".to_string())?;
            if !(0..=u16::MAX as i64).contains(&port) {
                return Err("Http.serve port is outside u16 range".to_string());
            }
            other("Net", "Listen", Some(port.to_string()))
        }
        ("Process", Some("run")) => other("Process", "Run", Some(string_arg(0)?)),
        ("System", Some("arg")) => {
            let index = regs
                .first()
                .and_then(|v| v.as_int())
                .ok_or_else(|| "System.arg requires an integer index".to_string())?;
            if index < 0 {
                return Err("System.arg index must be non-negative".to_string());
            }
            other("System", "Arg", Some(index.to_string()))
        }
        ("DB", Some("query")) => other("DB", "Query", None),
        ("Python", Some(op)) if !op.is_empty() => other("Python", op, None),
        ("Provider", Some("ask")) => other("Provider", "Ask", Some(string_arg(0)?)),
        ("Inference" | "LLM", Some("ask")) => other("Inference", "Ask", None),
        _ => return Ok(None),
    };
    Ok(Some(grant))
}

/// Parse an outbound HTTP URL into the exact TCP authority it needs. This is
/// deliberately small and fail-closed: only lowercase `http://` and
/// `https://` URLs are accepted, userinfo is rejected, and an ambiguous or
/// malformed authority never turns into a broader grant.
fn http_outbound_authority(url: &str) -> Result<crate::authority::AuthorityGrant, String> {
    let (rest, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443u16)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80u16)
    } else {
        return Err("outbound HTTP authority requires http:// or https://".to_string());
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or_else(|| "outbound HTTP URL has no authority".to_string())?;
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace) {
        return Err("outbound HTTP URL has invalid authority".to_string());
    }

    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| "unterminated bracketed HTTP host".to_string())?;
        let host = &bracketed[..close];
        if host.is_empty() {
            return Err("outbound HTTP host is empty".to_string());
        }
        let suffix = &bracketed[close + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            let raw = suffix
                .strip_prefix(':')
                .ok_or_else(|| "invalid bracketed HTTP authority suffix".to_string())?;
            parse_http_port(raw)?
        };
        (format!("[{host}]"), port)
    } else {
        if authority.matches(':').count() > 1 {
            return Err("IPv6 HTTP hosts must be bracketed".to_string());
        }
        match authority.rsplit_once(':') {
            Some((host, raw_port)) => {
                if host.is_empty() {
                    return Err("outbound HTTP host is empty".to_string());
                }
                (host.to_ascii_lowercase(), parse_http_port(raw_port)?)
            }
            None => (authority.to_ascii_lowercase(), default_port),
        }
    };

    if host.is_empty() {
        return Err("outbound HTTP host is empty".to_string());
    }
    Ok(crate::authority::AuthorityGrant::NetTcpOut { host, port })
}

fn parse_http_port(raw: &str) -> Result<u16, String> {
    let port = raw
        .parse::<u16>()
        .map_err(|_| "outbound HTTP port must be a valid u16".to_string())?;
    if port == 0 {
        return Err("outbound HTTP port must be non-zero".to_string());
    }
    Ok(port)
}

/// Enforce actor authority at the last common callback boundary before a host
/// operation executes. Actor-free runtime/top-level execution intentionally
/// retains its existing ambient behavior; #165 scopes this migration to
/// actor-backed authority. A missing actor or malformed manifest fails closed.
fn authorize_actor_host_effect(
    rt: &Runtime,
    actor_id: Option<u64>,
    effect_name: &str,
    op_name: Option<&str>,
    constants: &[crate::bytecode::Constant],
    regs: &[crate::vm::Value],
) -> Result<(), String> {
    let Some(actor_id) = actor_id else {
        return Ok(());
    };
    let Some(grant) = required_host_authority(effect_name, op_name, constants, regs)? else {
        return Ok(());
    };
    let actor = rt
        .actors
        .get(&actor_id)
        .ok_or_else(|| format!("authority source actor {actor_id} is missing"))?;
    actor.require_authority(&grant).map_err(|error| error.to_string())
}

'''
if text.count(anchor) != 1:
    raise SystemExit(f"host helper anchor drift: {text.count(anchor)}")
text = text.replace(anchor, helper + anchor, 1)

# RuntimeVmCallbacks: after test handlers, before any real host dispatch.
old = '''        {\n            let rt = self.runtime.borrow();\n            if let Some(result) = rt.check_test_handler(&qualified, regs) {\n                return Some(result);\n            }\n        }\n        if effect_name == "Otp" {'''
new = '''        {\n            let rt = self.runtime.borrow();\n            if let Some(result) = rt.check_test_handler(&qualified, regs) {\n                return Some(result);\n            }\n            if let Err(error) = authorize_actor_host_effect(\n                &rt,\n                rt.current_actor,\n                effect_name,\n                op_name,\n                &module.constants,\n                regs,\n            ) {\n                tracing::warn!(\n                    actor_id = ?rt.current_actor,\n                    effect = %qualified,\n                    %error,\n                    "denying actor-backed host effect"\n                );\n                return Some(crate::vm::Value::nil());\n            }\n        }\n        if effect_name == "Otp" {'''
text = replace_once(text, old, new, "RuntimeVmCallbacks host preflight")

# BytecodeRuntimeCallbacks: same boundary, using the fixed executing actor id.
old = '''            // Check test handlers before real dispatch.\n            if let Some(result) = (*self.runtime).check_test_handler(&qualified, regs) {\n                return Some(result);\n            }\n            if effect_name == "Otp" {'''
new = '''            // Check test handlers before real dispatch.\n            if let Some(result) = (*self.runtime).check_test_handler(&qualified, regs) {\n                return Some(result);\n            }\n            if let Err(error) = authorize_actor_host_effect(\n                &*self.runtime,\n                Some(self.actor_id),\n                effect_name,\n                op_name,\n                &module.constants,\n                regs,\n            ) {\n                tracing::warn!(\n                    actor_id = self.actor_id,\n                    effect = %qualified,\n                    %error,\n                    "denying actor-backed host effect"\n                );\n                return Some(crate::vm::Value::nil());\n            }\n            if effect_name == "Otp" {'''
text = replace_once(text, old, new, "BytecodeRuntimeCallbacks host preflight")

# Synchronous runtime-backed LLM path: actor calls require Inference::Ask;
# actor-free/top-level calls remain ambient.
old = '''    fn complete_llm(&mut self, model: &str, prompt: &str) -> Option<String> {\n        let mut rt = self.runtime.borrow_mut();\n        if let Some(actor_id) = rt.current_actor {'''
new = '''    fn complete_llm(&mut self, model: &str, prompt: &str) -> Option<String> {\n        let mut rt = self.runtime.borrow_mut();\n        if authorize_actor_host_effect(\n            &rt,\n            rt.current_actor,\n            "Inference",\n            Some("ask"),\n            &[],\n            &[],\n        )\n        .is_err()\n        {\n            return None;\n        }\n        if let Some(actor_id) = rt.current_actor {'''
text = replace_once(text, old, new, "RuntimeVmCallbacks LLM authority")

old = '''    fn complete_llm(&mut self, model: &str, prompt: &str) -> Option<String> {\n        unsafe {\n            let rt = &mut *self.runtime;\n            if rt\n                .actors'''
new = '''    fn complete_llm(&mut self, model: &str, prompt: &str) -> Option<String> {\n        unsafe {\n            let rt = &mut *self.runtime;\n            if authorize_actor_host_effect(\n                rt,\n                Some(self.actor_id),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],\n            )\n            .is_err()\n            {\n                return None;\n            }\n            if rt\n                .actors'''
text = replace_once(text, old, new, "BytecodeRuntimeCallbacks sync LLM authority")

# Async LLM path: authorize immediately before the first external dispatch.
old = '''            // Build the request on the scheduler thread, then hand it to a\n            // background worker for the HTTP call.\n            let is_agent = rt'''
new = '''            // Authority must still be valid immediately before the first\n            // externally-observable provider request. Completed/resumed calls\n            // above do not re-authorize an operation that already crossed the\n            // boundary.\n            if authorize_actor_host_effect(\n                rt,\n                Some(actor_id),\n                "Inference",\n                Some("ask"),\n                &[],\n                &[],\n            )\n            .is_err()\n            {\n                return PerformAsyncResult::Ready(None);\n            }\n\n            // Build the request on the scheduler thread, then hand it to a\n            // background worker for the HTTP call.\n            let is_agent = rt'''
text = replace_once(text, old, new, "async LLM authority before dispatch")

text += r'''

#[cfg(test)]
mod host_authority_tests {
    use super::{authorize_actor_host_effect, http_outbound_authority, required_host_authority};
    use crate::authority::{AuthorityGrant, AuthorityManifest};
    use crate::bytecode::Constant;
    use crate::runtime::{Actor, Runtime};
    use crate::vm::Value;

    fn string_args(values: &[&str]) -> (Vec<Constant>, Vec<Value>) {
        let constants: Vec<_> = values
            .iter()
            .map(|value| Constant::String((*value).to_string()))
            .collect();
        let regs = (0..values.len()).map(|idx| Value::string(idx as u32)).collect();
        (constants, regs)
    }

    #[test]
    fn http_authority_uses_exact_host_and_effective_port() {
        assert_eq!(
            http_outbound_authority("https://Api.Example.com/path").unwrap(),
            AuthorityGrant::NetTcpOut {
                host: "api.example.com".into(),
                port: 443,
            }
        );
        assert_eq!(
            http_outbound_authority("http://example.com:8080?q=1").unwrap(),
            AuthorityGrant::NetTcpOut {
                host: "example.com".into(),
                port: 8080,
            }
        );
        assert_eq!(
            http_outbound_authority("https://[2001:db8::1]/").unwrap(),
            AuthorityGrant::NetTcpOut {
                host: "[2001:db8::1]".into(),
                port: 443,
            }
        );
    }

    #[test]
    fn malformed_or_userinfo_http_targets_fail_closed() {
        for url in [
            "ftp://example.com/x",
            "https://user@example.com/x",
            "https://example.com:0/x",
            "https://2001:db8::1/x",
            "https:///x",
        ] {
            assert!(http_outbound_authority(url).is_err(), "{url} must be rejected");
        }
    }

    #[test]
    fn host_effects_map_to_typed_exact_grants() {
        let (constants, regs) = string_args(&["/tmp/input.txt"]);
        assert_eq!(
            required_host_authority("FS", Some("read"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::FsRead {
                path: "/tmp/input.txt".into(),
            })
        );

        let (constants, regs) = string_args(&["HOME"]);
        assert_eq!(
            required_host_authority("Env", Some("get"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::EnvRead { name: "HOME".into() })
        );

        let (constants, regs) = string_args(&["PAYMENTS_KEY"]);
        assert_eq!(
            required_host_authority("Secret", Some("read"), &constants, &regs).unwrap(),
            Some(AuthorityGrant::SecretRead {
                name: "PAYMENTS_KEY".into(),
            })
        );
    }

    #[test]
    fn actor_host_access_is_deny_by_default_and_exact_match_only() {
        let mut rt = Runtime::new();
        let actor_id = 800_001;
        rt.actors.insert(actor_id, Actor::new(actor_id, "host-authority", 8));
        let (constants, regs) = string_args(&["/tmp/allowed.txt"]);

        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_err());

        let manifest = AuthorityManifest::from_tokens(["Fs::Read(/tmp/allowed.txt)"]).unwrap();
        rt.actors
            .get_mut(&actor_id)
            .unwrap()
            .install_authority_manifest(&manifest);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_ok());

        let (other_constants, other_regs) = string_args(&["/tmp/other.txt"]);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "FS",
            Some("read"),
            &other_constants,
            &other_regs,
        )
        .is_err());
    }

    #[test]
    fn malformed_actor_manifest_cannot_authorize_exact_present_grant() {
        let mut rt = Runtime::new();
        let actor_id = 800_002;
        let mut actor = Actor::new(actor_id, "host-authority-invalid", 8);
        actor.capabilities.insert("Env::Read(HOME)".to_string());
        actor
            .capabilities
            .insert("Net::TcpOut(malformed)".to_string());
        rt.actors.insert(actor_id, actor);
        let (constants, regs) = string_args(&["HOME"]);

        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "Env",
            Some("get"),
            &constants,
            &regs,
        )
        .is_err());
    }

    #[test]
    fn actor_http_authority_is_exact_destination_not_category_permission() {
        let mut rt = Runtime::new();
        let actor_id = 800_003;
        let mut actor = Actor::new(actor_id, "host-network-authority", 8);
        let manifest = AuthorityManifest::from_tokens(["Net::TcpOut(api.example.com:443)"]).unwrap();
        actor.install_authority_manifest(&manifest);
        rt.actors.insert(actor_id, actor);

        let (constants, regs) = string_args(&["https://api.example.com/v1"]);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "Http",
            Some("get"),
            &constants,
            &regs,
        )
        .is_ok());

        let (constants, regs) = string_args(&["https://api.example.com:8443/v1"]);
        assert!(authorize_actor_host_effect(
            &rt,
            Some(actor_id),
            "Http",
            Some("get"),
            &constants,
            &regs,
        )
        .is_err());
    }

    #[test]
    fn actor_free_runtime_keeps_existing_ambient_host_contract() {
        let rt = Runtime::new();
        let (constants, regs) = string_args(&["/tmp/ambient.txt"]);
        assert!(authorize_actor_host_effect(
            &rt,
            None,
            "FS",
            Some("read"),
            &constants,
            &regs,
        )
        .is_ok());
    }
}
'''

path.write_text(text)
print("host authority boundary patch applied")

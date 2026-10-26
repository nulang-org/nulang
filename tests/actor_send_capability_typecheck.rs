//! Keep payload type matching separate from actor-message transfer authority.
//! These are compiler source tests, not assertions about an arbitrarily shared
//! `target/debug/nulang` binary from another Cargo worktree.
use nulang::ast::Decl;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn compile_actor_send(capability: &str, value: &str) -> Result<(), String> {
    let src = format!(
        "actor Sink {{ behavior recv(x: Int) {{ perform IO.print(x) }} }}\n\
         fn main() {{ let s = spawn Sink {{}}; let v = {value} :cap {capability}; send s recv(v); unit }}"
    );
    compile_source(&src)
}

fn compile_source(src: &str) -> Result<(), String> {
    let lexed = Lexer::new(src).lex().map_err(|e| e.to_string())?;
    let ast = Parser::new(lexed)
        .parse_module()
        .map_err(|e| e.to_string())?;
    TypeChecker::new()
        .check_module(&ast)
        .map_err(|e| format!("type: {e}"))?;
    EffectChecker::new()
        .check_module(&ast.decls)
        .map_err(|e| format!("effect: {e}"))?;
    // Mirrors main.rs's independent capability-analysis pass: EffectChecker
    // analyzes effect rows; it does not enforce sendability on its own.
    let mut caps = CapabilityAnalyzer::new();
    for decl in &ast.decls {
        if let Decl::Function { body, .. } = decl {
            caps.infer_cap(&CapContext::new(), body)
                .map_err(|e| format!("cap: {e}"))?;
        }
    }
    Ok(())
}

#[test]
fn transferable_primitive_capabilities_keep_their_int_payload() {
    for cap in ["iso", "tag", "lineariso", "val"] {
        compile_actor_send(cap, "42")
            .unwrap_or_else(|err| panic!("{cap} Int must be sendable: {err}"));
    }
}

#[test]
fn forbidden_capabilities_are_rejected_by_capability_checker() {
    for cap in ["ref", "box", "trn"] {
        let err = compile_actor_send(cap, "42").expect_err("actor transfer must fail");
        assert!(
            err.starts_with("cap:") && err.contains(cap),
            "{cap} should be rejected by capability checker, got {err}"
        );
    }
}

#[test]
fn sender_capability_does_not_erase_payload_type() {
    let err = compile_actor_send("iso", "\"incorrect\"").expect_err("String is not Int");
    assert!(err.starts_with("type:"), "expected type error: {err}");
}

#[test]
fn coercing_ref_to_opaque_tag_preserves_payload_type() {
    // This existing conformance case has two nested capability annotations.
    // Sending its opaque tag should check the Int payload, not try to unify
    // &ref Int with the behavior's plain Int parameter.
    compile_source(include_str!(
        "../conformance/behavior/cap_10_coerce_ref_to_tag_send.nula"
    ))
    .expect("tag attenuation must typecheck and be sendable");
}

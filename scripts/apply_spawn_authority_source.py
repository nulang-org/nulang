#!/usr/bin/env python3
"""One-shot patch for source-level spawn authority grants.

This deliberately fails closed if the parser/lowering anchors moved. The patch
keeps typed authority validation at parse time and preserves canonical tokens
through AST -> HIR -> MIR; exact-PC bytecode/runtime provenance is applied by
the companion authority provenance patch before this script runs.
"""
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one anchor, found {count}: {old[:120]!r}")
    p.write_text(text.replace(old, new, 1))


# Parser: validate typed grants immediately, canonicalize/dedupe the metadata,
# and keep authority arguments compile-time literal-only.
replace_once(
    "src/parser.rs",
    "use crate::ast::*;\n",
    "use crate::ast::*;\nuse crate::authority::AuthorityGrant;\n",
)

old_registration = '''        // Optional named registration: `spawn Foo() as "name"`
        let register_as = if self.consume_if(&TokenKind::As) {
            Some(self.expect_string("actor name")?)
        } else {
            None
        };
'''
new_registration = '''        // Optional external-authority attenuation for the child. Grants are
        // intentionally structural and literal-only: authority must be known at
        // compile time, never computed from ambient runtime data.
        //
        //   spawn Worker() with [
        //       Net::TcpOut("api.example.com:443"),
        //       Secret::Read("stripe_key"),
        //   ]
        let capabilities = if self.consume_if(&TokenKind::With) {
            self.expect(TokenKind::LBracket)?;
            self.skip_newlines();
            let mut grants = Vec::new();
            while !self.match_token(&TokenKind::RBracket) && !self.is_at_end() {
                let grant_span = self.current_span();
                let namespace = self.expect_ident("authority namespace")?;
                self.expect(TokenKind::DoubleColon)?;
                let operation = self.expect_ident("authority operation")?;
                let argument = if self.consume_if(&TokenKind::LParen) {
                    let argument = self.expect_string("authority argument")?;
                    self.expect(TokenKind::RParen)?;
                    Some(argument)
                } else {
                    None
                };

                let grant = AuthorityGrant::from_parts(
                    &namespace,
                    &operation,
                    argument.as_deref(),
                )
                .map_err(|err| NuError::parse_error(err.to_string(), grant_span))?;
                grants.push(grant.to_string());

                self.skip_newlines();
                if !self.consume_if(&TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
            self.expect(TokenKind::RBracket)?;
            // Canonical metadata makes equivalent source produce identical
            // artifact identity regardless of grant order or duplication.
            grants.sort_unstable();
            grants.dedup();
            grants
        } else {
            Vec::new()
        };

        // Optional named registration: `spawn Foo() as "name"`.
        // Canonical order is `spawn Foo() with [...] as "name"`.
        let register_as = if self.consume_if(&TokenKind::As) {
            Some(self.expect_string("actor name")?)
        } else {
            None
        };
'''
replace_once("src/parser.rs", old_registration, new_registration)

replace_once(
    "src/parser.rs",
    '''            target_node,
            // Spawn-time capability-grant syntax (`with [Net::TcpOut(...)]`) is
            // not parsed yet; the field is plumbed through as empty.
            capabilities: Vec::new(),
            span,
''',
    '''            target_node,
            capabilities,
            span,
''',
)

# HIR lowering previously erased the AST field. Preserve the canonical tokens.
replace_once(
    "src/hir_lower.rs",
    '''        Expr::Spawn {
            actor_type,
            init,
            target_node,
            span,
            ..
        } => {
''',
    '''        Expr::Spawn {
            actor_type,
            init,
            target_node,
            capabilities,
            span,
            ..
        } => {
''',
)
replace_once(
    "src/hir_lower.rs",
    '''                    target_node: target_operand,
                    capabilities: vec![],
                    ty: ty.clone(),
''',
    '''                    target_node: target_operand,
                    capabilities: capabilities.clone(),
                    ty: ty.clone(),
''',
)

# Extend the exact-PC integration suite so source syntax, not test-side MIR
# mutation, proves the complete frontend -> runtime metadata path.
test_path = Path("tests/spawn_authority_provenance.rs")
text = test_path.read_text()
anchor = '''#[test]
fn codegen_records_distinct_grants_by_exact_spawn_pc() {
'''
if text.count(anchor) != 1:
    raise RuntimeError("spawn authority test anchor moved")
source_tests = r'''#[test]
fn source_grants_survive_frontend_and_bind_to_exact_spawn_pc() {
    let mut mir = lower(
        r#"
actor Child {
    behavior ping() { 1 }
}
fn main() {
    let first = spawn Child {} with [Secret::Read("FIRST_KEY")]
    let second = spawn Child {} with [Secret::Read("SECOND_KEY")]
    second
}
"#,
    );
    let module = compile_mir(&mut mir, "authority-source-pc").unwrap();
    let spawn_pcs: Vec<_> = module
        .instructions
        .iter()
        .enumerate()
        .filter_map(|(pc, instr)| (instr.opcode == OpCode::Spawn).then_some(pc))
        .collect();
    assert_eq!(spawn_pcs.len(), 2);
    assert_eq!(
        spawn_grants(&module, spawn_pcs[0]),
        vec!["Secret::Read(FIRST_KEY)"]
    );
    assert_eq!(
        spawn_grants(&module, spawn_pcs[1]),
        vec!["Secret::Read(SECOND_KEY)"]
    );
}

#[test]
fn source_grants_are_canonicalized_and_deduplicated() {
    let mut mir = lower(
        r#"
actor Child { behavior ping() { 1 } }
fn main() {
    spawn Child {} with [
        Secret::Read("B"),
        Fs::Read("/tmp/input"),
        Secret::Read("B"),
        Env::Read("HOME")
    ]
}
"#,
    );
    let module = compile_mir(&mut mir, "authority-source-canonical").unwrap();
    let spawn_pc = module
        .instructions
        .iter()
        .position(|instr| instr.opcode == OpCode::Spawn)
        .unwrap();
    assert_eq!(
        spawn_grants(&module, spawn_pc),
        vec![
            "Env::Read(HOME)",
            "Fs::Read(/tmp/input)",
            "Secret::Read(B)",
        ]
    );
}

#[test]
fn malformed_source_grant_fails_during_parse() {
    let source = r#"
actor Child { behavior ping() { 1 } }
fn main() {
    spawn Child {} with [Net::TcpOut("api.example.com")]
}
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    let error = Parser::new(tokens).parse_module().unwrap_err();
    assert!(error.to_string().contains("TcpOut expects host:port"));
}

#[test]
fn dynamic_source_grant_argument_is_rejected() {
    let source = r#"
actor Child { behavior ping() { 1 } }
fn main() {
    let key = "KEY"
    spawn Child {} with [Secret::Read(key)]
}
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    assert!(Parser::new(tokens).parse_module().is_err());
}

#[test]
fn privileged_remote_spawn_from_source_fails_closed() {
    let mut mir = lower(
        r#"
actor Child { behavior ping() { 1 } }
fn main() {
    let node1 = 7
    spawn@node1 Child {} with [Secret::Read("KEY")]
}
"#,
    );
    let error = compile_mir(&mut mir, "authority-source-remote").unwrap_err();
    assert!(error
        .to_string()
        .contains("distributed spawn protocol carries typed authority"));
}

'''
test_path.write_text(text.replace(anchor, source_tests + anchor, 1))

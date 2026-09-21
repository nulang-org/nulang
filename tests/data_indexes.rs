use nulang::hir::Decl as HirDecl;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn parse(source: &str) -> nulang::types::NuResult<nulang::ast::AstModule> {
    let tokens = Lexer::new(source).lex()?;
    Parser::new(tokens).parse_module()
}

fn check(source: &str) -> nulang::types::NuResult<nulang::hir::Module> {
    let ast = parse(source)?;
    let mut tc = TypeChecker::new();
    tc.check_module(&ast)?;
    Ok(nulang::hir_lower::lower_module(
        &ast,
        &tc.inferred_decl_types,
    ))
}

#[test]
fn typed_entity_indexes_survive_hir_lowering() {
    let hir = check(
        r#"entity Customer {
            state email: String = "a@example.com"
            state company: String = "Acme"
            state status: String = "active"
            unique index email
            index by_company_status { company, status }
        }"#,
    )
    .expect("indexed entity should typecheck");

    let customer = hir
        .decls
        .iter()
        .find_map(|decl| match decl {
            HirDecl::Actor(actor) if actor.name == "Customer" => Some(actor),
            _ => None,
        })
        .expect("Customer actor");

    assert_eq!(customer.indexes.len(), 2);
    assert!(customer.indexes[0].unique);
    assert_eq!(customer.indexes[0].fields, vec!["email"]);
    assert!(!customer.indexes[1].unique);
    assert_eq!(
        customer.indexes[1].fields,
        vec!["company".to_string(), "status".to_string()]
    );
}

#[test]
fn index_rejects_unknown_state_field() {
    let err = check(
        r#"entity Customer {
            state email: String = "a@example.com"
            index by_company { company }
        }"#,
    )
    .expect_err("unknown indexed field must fail");

    assert!(
        err.to_string().contains("unknown state field 'company'"),
        "unexpected error: {err}"
    );
}

#[test]
fn index_rejects_ephemeral_local_state() {
    let err = check(
        r#"entity Session {
            state local cache: Int = 0
            index cache
        }"#,
    )
    .expect_err("local state cannot be indexed");

    assert!(
        err.to_string()
            .contains("cannot reference local field 'cache'"),
        "unexpected error: {err}"
    );
}

#[test]
fn duplicate_index_names_fail_typechecking() {
    let err = check(
        r#"entity Customer {
            state email: String = "a@example.com"
            index email
            index email
        }"#,
    )
    .expect_err("duplicate index names must fail");

    assert!(
        err.to_string().contains("duplicate index 'email'"),
        "unexpected error: {err}"
    );
}

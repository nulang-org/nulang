use std::collections::HashSet;
use std::path::Path;

use nulang::ast::Decl;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn typecheck_example_with_imports(path: &Path) -> nulang::types::NuResult<nulang::types::Type> {
    let source = std::fs::read_to_string(path).map_err(|e| nulang::types::NuError::VMError {
        msg: format!("cannot read {}: {}", path.display(), e),
        span: nulang::types::Span::default(),
    })?;

    let prelude = nulang::prelude_source::PRELUDE_SOURCE;
    let prelude_tokens = Lexer::new(prelude).lex()?;
    let prelude_ast = Parser::new(prelude_tokens).parse_module()?;

    let tokens = Lexer::new(&source).lex()?;
    let mut ast = Parser::new(tokens).parse_module()?;

    let mut prelude_variants: Vec<Decl> = prelude_ast
        .decls
        .into_iter()
        .filter(|decl| matches!(decl, Decl::VariantType { .. }))
        .collect();
    prelude_variants.append(&mut ast.decls);
    ast.decls = prelude_variants;

    let mut visited = HashSet::new();
    nulang::resolver::resolve_imports(&mut ast, path, &mut visited)?;

    TypeChecker::new().check_module(&ast)
}

#[test]
fn imported_stdlib_json_example_typechecks_without_recursive_overflow() {
    // This intentionally exercises the real import-resolved JsonValue graph used
    // by examples/12_json.nula. A smaller hand-written JsonValue declaration is
    // not sufficient: the current draft passes that test while the full example
    // runner overflows a 16 MiB stack on this example.
    let handle = std::thread::Builder::new()
        .name("json-value-restriction-regression".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples")
                .join("12_json.nula");
            assert!(
                path.exists(),
                "missing regression fixture: {}",
                path.display()
            );

            let result = typecheck_example_with_imports(&path);
            assert!(
                result.is_ok(),
                "import-resolved JSON example must typecheck: {:?}",
                result.err()
            );
        })
        .expect("spawn focused JSON typecheck regression thread");

    handle
        .join()
        .expect("focused JSON typecheck regression thread panicked");
}

use nulang::behavior_manifest::{
    ArtifactKind, AuthorityKind, BehaviorManifest, HostAbiRequirement, ManifestBuildInput,
};
use nulang::effect_checker::EffectChecker;
use nulang::lexer::Lexer;
use nulang::parser::Parser;

fn checked(source: &str) -> (nulang::ast::AstModule, EffectChecker) {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = EffectChecker::new();
    checker.check_module(&ast.decls).expect("effect check");
    (ast, checker)
}

fn manifest(source: &str) -> BehaviorManifest {
    let (ast, mut checker) = checked(source);
    BehaviorManifest::from_checked_module(
        ManifestBuildInput {
            package_name: "authority-test",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"artifact",
            compiler_implementation: "nulang-rust",
            compiler_version: "test",
            compiler_bytes: b"compiler",
            host_abi: HostAbiRequirement {
                schema: nulang::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                required_operations: Vec::new(),
                requires_legacy_extension_dispatch: false,
            },
            source_bytes: source.as_bytes(),
            dependency_bytes: b"",
        },
        &mut checker,
        &ast.decls,
    )
    .expect("manifest")
}

#[test]
fn inferred_resource_effects_cannot_disappear_from_authority_requirements() {
    let manifest = manifest(
        r#"
        fn fetch() { perform Http.get("https://example.com") }
        fn load() { perform FS.read("/tmp/input") }
        fn main() { fetch(); load() }
        "#,
    );

    let network = manifest
        .authority
        .iter()
        .find(|decl| decl.kind == AuthorityKind::Network)
        .expect("network authority requirement");
    assert!(network.required);
    assert_eq!(network.resource.as_deref(), Some("*"));
    assert_eq!(network.operations, vec!["connect"]);

    let filesystem = manifest
        .authority
        .iter()
        .find(|decl| decl.kind == AuthorityKind::Filesystem)
        .expect("filesystem authority requirement");
    assert!(filesystem.required);
    assert_eq!(filesystem.resource.as_deref(), Some("*"));
    assert_eq!(filesystem.operations, vec!["read", "write"]);
}

#[test]
fn artifact_identity_is_content_bound_not_filename_bound() {
    let source = "fn main() { 1 }";
    let (ast, mut checker_a) = checked(source);
    let (_, mut checker_b) = checked(source);

    let first = BehaviorManifest::from_checked_module(
        ManifestBuildInput {
            package_name: "content-test",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"artifact-a",
            compiler_implementation: "nulang-rust",
            compiler_version: "test",
            compiler_bytes: b"compiler",
            host_abi: HostAbiRequirement {
                schema: nulang::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                required_operations: Vec::new(),
                requires_legacy_extension_dispatch: false,
            },
            source_bytes: source.as_bytes(),
            dependency_bytes: b"",
        },
        &mut checker_a,
        &ast.decls,
    )
    .unwrap();
    let second = BehaviorManifest::from_checked_module(
        ManifestBuildInput {
            package_name: "content-test",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"artifact-b",
            compiler_implementation: "nulang-rust",
            compiler_version: "test",
            compiler_bytes: b"compiler",
            host_abi: HostAbiRequirement {
                schema: nulang::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                required_operations: Vec::new(),
                requires_legacy_extension_dispatch: false,
            },
            source_bytes: source.as_bytes(),
            dependency_bytes: b"",
        },
        &mut checker_b,
        &ast.decls,
    )
    .unwrap();

    assert_ne!(first.artifact.digest, second.artifact.digest);
}

use nulang::behavior_manifest::{
    ArtifactKind, AuthorityKind, BehaviorManifest, ManifestBuildInput, ReplayClass,
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

    let net_replay = manifest
        .replay
        .iter()
        .find(|decl| decl.effect == "Net")
        .expect("network replay classification");
    assert_eq!(
        net_replay.class,
        ReplayClass::ExternalRequiresIdempotencyKey
    );

    let fs_replay = manifest
        .replay
        .iter()
        .find(|decl| decl.effect == "FS")
        .expect("filesystem replay classification");
    assert_eq!(fs_replay.class, ReplayClass::ExternalRequiresIdempotencyKey);
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
            source_bytes: source.as_bytes(),
            dependency_bytes: b"",
        },
        &mut checker_b,
        &ast.decls,
    )
    .unwrap();

    assert_ne!(first.artifact.digest, second.artifact.digest);
}

#[test]
fn actor_protocol_manifest_is_structural_and_order_independent() {
    let first = manifest(
        r#"
        actor Account {
            behavior deposit(amount: Int) -> Unit { nil }
            behavior balance() -> Int { 0 }
        }
        "#,
    );
    let second = manifest(
        r#"
        actor Account {
            behavior balance() -> Int { 0 }
            behavior deposit(amount: Int) -> Unit { nil }
        }
        "#,
    );

    assert_eq!(first.actors.len(), 1);
    assert_eq!(second.actors.len(), 1);
    let first_protocol = first.actors[0]
        .protocol
        .as_ref()
        .expect("fully annotated actor protocol");
    let second_protocol = second.actors[0]
        .protocol
        .as_ref()
        .expect("fully annotated actor protocol");
    assert_eq!(first_protocol, second_protocol);
    assert!(first_protocol.starts_with("nulang.protocol/v1:"));
}

#[test]
fn actor_protocol_manifest_changes_when_signature_changes() {
    let int_version = manifest(
        r#"
        actor Account {
            behavior deposit(amount: Int) -> Unit { nil }
        }
        "#,
    );
    let string_version = manifest(
        r#"
        actor Account {
            behavior deposit(amount: String) -> Unit { nil }
        }
        "#,
    );

    assert_ne!(
        int_version.actors[0].protocol, string_version.actors[0].protocol,
        "wire-visible behavior signature changes must change protocol identity"
    );
}

#[test]
fn actor_protocol_manifest_fails_closed_on_incomplete_signatures() {
    let manifest = manifest(
        r#"
        actor Account {
            behavior deposit(amount) -> Unit { nil }
        }
        "#,
    );

    assert_eq!(manifest.actors.len(), 1);
    assert_eq!(manifest.actors[0].name, "Account");
    assert!(
        manifest.actors[0].protocol.is_none(),
        "the manifest must not guess a protocol identity from an untyped parameter"
    );
}

#[test]
fn actor_manifest_is_sorted_by_actor_name() {
    let manifest = manifest(
        r#"
        actor Zebra {
            behavior ping() -> Unit { nil }
        }
        actor Alpha {
            behavior ping() -> Unit { nil }
        }
        "#,
    );

    let names: Vec<_> = manifest
        .actors
        .iter()
        .map(|actor| actor.name.as_str())
        .collect();
    assert_eq!(names, vec!["Alpha", "Zebra"]);
}

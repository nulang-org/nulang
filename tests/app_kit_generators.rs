//! Regression tests for the first Laravel-inspired application generator.
//! These tests specify behavior before the implementation is introduced.

use nulang::package::generators::generate;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct PackageFixture {
    root: PathBuf,
}

impl PackageFixture {
    fn new(label: &str) -> Self {
        let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nulang_app_kit_{label}_{}_{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("Nulang.toml"),
            "[package]\nname = \"app-kit-test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for PackageFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn feature_generates_a_module_and_an_executable_test_fixture() {
    let package = PackageFixture::new("feature");
    let files = generate(package.root(), "feature", "customer_profile").unwrap();
    assert_eq!(files.len(), 2);

    let module = std::fs::read_to_string(package.root().join("src/customer_profile.nula")).unwrap();
    let test = std::fs::read_to_string(package.root().join("tests/test_customer_profile.nula")).unwrap();
    assert!(module.contains("pub fn customer_profile_label() -> String"));
    assert!(test.contains("import customer_profile"));
    assert!(test.contains("fn test_customer_profile_label()"));
    assert!(test.contains("customer_profile_label() == \"customer_profile\""));
}

#[test]
fn existing_test_is_never_overwritten_or_partially_scaffolded() {
    let package = PackageFixture::new("conflict");
    std::fs::create_dir_all(package.root().join("tests")).unwrap();
    let existing = package.root().join("tests/test_customer.nula");
    std::fs::write(&existing, "KEEP MY TEST").unwrap();

    let err = generate(package.root(), "feature", "customer").unwrap_err();
    assert!(err.to_string().contains("already exists"));
    assert_eq!(std::fs::read_to_string(existing).unwrap(), "KEEP MY TEST");
    assert!(!package.root().join("src/customer.nula").exists());
}

#[test]
fn rejects_invalid_or_unsafe_feature_names() {
    let package = PackageFixture::new("invalid");
    for name in ["", "../escape", "nested/path", "customer-name", "8customer", "Customer", "main", "lib", "_hidden"] {
        assert!(
            generate(package.root(), "feature", name).is_err(),
            "should reject {name:?}"
        );
    }
    assert!(!package.root().join("src").exists());
}

#[test]
fn generator_requires_an_existing_valid_package_and_known_kind() {
    let package = PackageFixture::new("package");
    assert!(generate(package.root(), "unknown", "customer").is_err());

    std::fs::remove_file(package.root().join("Nulang.toml")).unwrap();
    assert!(generate(package.root(), "feature", "customer").is_err());
    assert!(!package.root().join("src/customer.nula").exists());
}

#[test]
fn standalone_module_and_test_generators_use_the_same_conventions() {
    let package = PackageFixture::new("standalone");
    let module_files = generate(package.root(), "module", "invoice").unwrap();
    let test_files = generate(package.root(), "test", "invoice").unwrap();
    assert_eq!(module_files.len(), 1);
    assert_eq!(test_files.len(), 1);
    assert!(package.root().join("src/invoice.nula").exists());
    assert!(package.root().join("tests/test_invoice.nula").exists());

    assert!(generate(package.root(), "module", "invoice").is_err());
    assert!(generate(package.root(), "test", "invoice").is_err());
}

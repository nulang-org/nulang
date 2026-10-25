//! Executable contract for the two-resource, tenant-bound scaffold example.
//! This checks actual Nulang execution and exact generated source, not text
//! matching against an independent mock policy engine.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/tenant-resources")
}

fn run_in(exe: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new(exe)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("could not run {args:?}: {error}"))
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: exit={}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

struct ScratchDir(PathBuf);
impl ScratchDir {
    fn new(label: &str) -> Self {
        let name = format!("nulang_tenant_reference_{label}_{}", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create isolated fixture directory");
        Self(path)
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn two_resource_policies_execute_and_deny_cross_tenant_reads_and_writes() {
    let output = run_in(
        Path::new(env!("CARGO_BIN_EXE_nulang")),
        &example_dir(),
        &["nula", "test", "--verbose"],
    );
    assert_success(&output, "two-tenant Nulang policy test suite");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 passed"),
        "test suite did not report passing test: {stdout}"
    );
}

#[test]
fn checked_in_resources_are_exact_generator_output_and_typecheck() {
    let exe = Path::new(env!("CARGO_BIN_EXE_nulang"));
    let root = ScratchDir::new("scaffold");
    fs::write(
        root.0.join("Nulang.toml"),
        "[package]\nname = \"tenant-reference-generator\"\nversion = \"0.1.0\"\n",
    )
    .expect("write disposable package manifest");

    let resources: [(&str, &[&str], &str); 2] = [
        (
            "BillboardFace",
            &["--field", "name:String", "--field", "active:Bool"],
            "billboard_face.nula",
        ),
        (
            "Campaign",
            &["--field", "title:String", "--field", "face_id:String"],
            "campaign.nula",
        ),
    ];
    for (name, fields, stem) in resources {
        let mut args = vec!["nula", "generate", "resource", name];
        args.extend_from_slice(fields);
        assert_success(&run_in(exe, &root.0, &args), "generate reference resource");
        let generated = root.0.join("src/resources").join(stem);
        let committed = example_dir().join("src/resources").join(stem);
        assert_eq!(
            fs::read_to_string(&generated).expect("read generated source"),
            fs::read_to_string(&committed).expect("read reference source"),
            "{name} source drifted from generator; update the example deliberately"
        );
        let generated_path = generated.to_str().expect("temporary source path is UTF-8");
        assert_success(
            &run_in(exe, &root.0, &["--check", generated_path]),
            "typecheck generator output",
        );
        // Repeated generation must refuse overwrite, including in a sample app.
        assert!(!run_in(exe, &root.0, &args).status.success());
    }
}

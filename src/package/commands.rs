//! `nula` CLI subcommands: `new`, `init`, `build`, `build-wasm`, `test`, `run`,
//! `list`, `clean`, `add`, `remove`, `watch`, `publish`, `deploy`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::package::lockfile::{Lockfile, LOCKFILE_FILE};
use crate::package::manifest::{Dependency, DependencyDetail, Manifest, MANIFEST_FILE};
use crate::package::resolver::resolve;
use crate::types::{NuError, NuResult, Span};
use crate::web::modules::{ModuleRegistry, ModuleSpec};

use crate::bytecode::CodeModule;
use crate::runtime::{render_route_handler, WebDevServer};
use crate::vm::VM;

use crate::registry::RegistryClient;

thread_local! {
    /// Optional per-thread override for the package root, set by tests to
    /// avoid mutating the process-global working directory. `set_current_dir`
    /// in one test raced with unrelated parallel tests resolving `stdlib::*`
    /// and example files relative to `current_dir()`, causing random
    /// cross-test failures that vanished in isolation.
    static PACKAGE_ROOT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        std::cell::RefCell::new(None);
}

/// The base directory a `cmd_*` function operates in: a per-thread test
/// override if present (see `PACKAGE_ROOT_OVERRIDE`), else the process
/// working directory. Never mutates global CWD state.
fn package_root() -> NuResult<PathBuf> {
    if let Some(dir) = PACKAGE_ROOT_OVERRIDE.with(|c| c.borrow().clone()) {
        return Ok(dir);
    }
    std::env::current_dir().map_err(|e| NuError::PackageError {
        msg: format!("cannot read current directory: {}", e),
        span: Span::default(),
    })
}

/// Dispatch a `nula` invocation (`args` excludes the leading `nula`).
pub fn run(args: &[String]) -> NuResult<()> {
    // Module-contributed subcommands (`@nulang/auth enable`, etc.) take
    // precedence over built-in commands so modules can extend the CLI.
    if let Some(result) = try_module_subcommand(args) {
        return result;
    }

    match args.first().map(String::as_str) {
        Some("new") => {
            let mut template: Option<&str> = None;
            let mut path_arg: Option<&str> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--template" => {
                        i += 1;
                        if i < args.len() {
                            template = Some(args[i].as_str());
                        }
                    }
                    other => {
                        if path_arg.is_some() {
                            return Err(NuError::PackageError {
                                msg: format!("unexpected argument '{}' for nula new", other),
                                span: Span::default(),
                            });
                        }
                        path_arg = Some(other);
                    }
                }
                i += 1;
            }
            cmd_new(path_arg, template)
        }
        Some("init") => cmd_init(),
        Some("build") => {
            let web = args.get(1).map(String::as_str) == Some("--web");
            if web {
                cmd_build_web()
            } else {
                let json = args[1..].iter().any(|a| a == "--json");
                if args.len() > 1 && !json {
                    return Err(NuError::PackageError {
                        msg: format!("unknown flag '{}' for nula build", args[1]),
                        span: Span::default(),
                    });
                }
                cmd_build(json)
            }
        }
        Some("build-wasm") => cmd_build_wasm(),
        Some("test") => {
            let mut filter: Option<&str> = None;
            let mut verbose = false;
            let mut watch = false;
            let mut json = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--filter" => {
                        i += 1;
                        if i < args.len() {
                            filter = Some(args[i].as_str());
                        }
                    }
                    "--verbose" | "-v" => verbose = true,
                    "--watch" | "-w" => watch = true,
                    "--json" => json = true,
                    other => {
                        return Err(NuError::PackageError {
                            msg: format!("unknown flag '{}' for nula test", other),
                            span: Span::default(),
                        });
                    }
                }
                i += 1;
            }
            if watch {
                cmd_test_watch(filter, verbose)
            } else {
                cmd_test(filter, verbose, json)
            }
        }
        Some("run") => {
            if args.get(1).map(String::as_str) == Some("--watch") {
                cmd_run_watch()
            } else {
                cmd_run()
            }
        }
        Some("watch") => cmd_run_watch(),
        Some("dev") => {
            let mut port: Option<u16> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--port" => {
                        i += 1;
                        if i < args.len() {
                            port = Some(args[i].parse().map_err(|_| NuError::PackageError {
                                msg: format!("invalid port '{}'", args[i]),
                                span: Span::default(),
                            })?);
                        }
                    }
                    other => {
                        return Err(NuError::PackageError {
                            msg: format!("unexpected argument '{}' for nula dev", other),
                            span: Span::default(),
                        });
                    }
                }
                i += 1;
            }
            cmd_dev(port)
        }
        Some("add") => {
            let name = args.get(1);
            let mut path: Option<String> = None;
            let mut git: Option<String> = None;
            let mut version: Option<String> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--path" => {
                        i += 1;
                        if i < args.len() {
                            path = Some(args[i].clone());
                        }
                    }
                    "--git" => {
                        i += 1;
                        if i < args.len() {
                            git = Some(args[i].clone());
                        }
                    }
                    "--version" => {
                        i += 1;
                        if i < args.len() {
                            version = Some(args[i].clone());
                        }
                    }
                    other => {
                        return Err(NuError::PackageError {
                            msg: format!("unknown flag '{}' for nula add", other),
                            span: Span::default(),
                        });
                    }
                }
                i += 1;
            }
            cmd_add(name, path.as_deref(), git.as_deref(), version.as_deref())
        }
        Some("remove") => cmd_remove(args.get(1).map(String::as_str)),
        Some("list") => cmd_list(),
        Some("clean") => cmd_clean(),
        Some("doc") => {
            let open = args.get(1).map(String::as_str) == Some("--open");
            cmd_doc(open)
        }
        Some("publish") => {
            let mut registry_url: Option<String> = None;
            let mut token: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--registry" => {
                        i += 1;
                        if i < args.len() { registry_url = Some(args[i].clone()); }
                    }
                    "--token" => {
                        i += 1;
                        if i < args.len() { token = Some(args[i].clone()); }
                    }
                    other => {
                        return Err(NuError::PackageError {
                            msg: format!("unknown flag '{}' for nula publish", other),
                            span: Span::default(),
                        });
                    }
                }
                i += 1;
            }
            cmd_publish(registry_url, token)
        }

        Some("deploy") => {
            let mut token: Option<String> = None;
            let mut url: Option<String> = None;
            let mut wasm = false;
            let mut adapter: Option<String> = None;
            let mut dry_run = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--token" => {
                        i += 1;
                        if i < args.len() { token = Some(args[i].clone()); }
                    }
                    "--url" => {
                        i += 1;
                        if i < args.len() { url = Some(args[i].clone()); }
                    }
                    "--wasm" => wasm = true,
                    "--adapter" => {
                        i += 1;
                        if i < args.len() { adapter = Some(args[i].clone()); }
                    }
                    "--dry-run" => dry_run = true,
                    other => return Err(NuError::PackageError {
                        msg: format!("unknown flag '{}' for nula deploy", other),
                        span: Span::default()
                    })
                }
                i += 1;
            }
            let adapter = adapter
                .as_deref()
                .and_then(crate::web::adapters::AdapterKind::from_str)
                .unwrap_or(crate::web::adapters::AdapterKind::NulangCloud);
            cmd_deploy(wasm, url, token, adapter, dry_run)
        }

        Some(other) => Err(NuError::PackageError {
            msg: format!(
                "unknown nula subcommand '{}' (expected new, init, build, build-wasm, test, run, add, remove, publish, deploy, watch, doc, list, or clean)",
                other
            ),
            span: Span::default(),
        }),
        None => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    println!("nula — the Nulang package manager");
    println!();
    println!("Usage: nulang nula <COMMAND>");
    println!();
    println!("Commands:");
    println!("  new   <path> [--template <name>]");
    println!("                Scaffold a new package directory");
    println!("                Templates: default, cli, lib, full");
    println!("  init          Scaffold a new package in the current directory");
    println!("  build         Build the package (type-check + .nbc artifact in .nula/dist/)");
    println!("  build-wasm    Build package to .wasm + .cwasm in .nula/dist/");
    println!("  test [--filter <substr>] [--verbose|-v] [--watch|-w]  Run .nula test files");
    println!("  run           Build and run the package entry point");
    println!("  run --watch   Build and re-run on source changes");
    println!("  watch         Alias for 'run --watch'");
    println!("  add   <name>  Add a dependency to Nulang.toml");
    println!("  remove <name> Remove a dependency from Nulang.toml");
    println!("  publish       Publish the package to a registry");
    println!("                --registry <url>  Registry URL (or set in Nulang.toml)");
    println!("                --token <token>   Auth token (or set NULA_TOKEN)");
    println!("  deploy        Build and deploy the package to Nulang Cloud");
    println!("                --wasm          Also bundle .wasm + .cwasm artifacts");
    println!("                --url <url>     Cloud API URL (or set NULANG_CLOUD_URL)");
    println!("                --token <token> Auth token (or set NULANG_CLOUD_TOKEN)");
    println!("  list          List resolved dependencies from Nulang.lock");
    println!("  clean         Remove build artifacts (.nula/dist/)");
    println!("  doc [--open]  Generate Markdown API docs (docs/api.md)");
}

/// If `args` matches a registered `@nulang/*` module subcommand, run it.
fn try_module_subcommand(args: &[String]) -> Option<NuResult<()>> {
    let registry = ModuleRegistry::builtin();
    let cmd = args.join(" ");
    for (module, spec) in &registry.modules {
        if spec.cli_subcommands.iter().any(|s| s == &cmd) {
            return Some(run_module_subcommand(module, spec, args));
        }
    }
    None
}

/// Execute a module-specific CLI subcommand.
fn run_module_subcommand(module: &str, spec: &ModuleSpec, args: &[String]) -> NuResult<()> {
    match module {
        "@nulang/auth" => run_auth_enable(args),
        _ => {
            println!("{} subcommand '{}' registered.", module, args.join(" "));
            println!("Capabilities: {:?}", spec.capabilities);
            println!("Cloud config keys: {:?}", spec.cloud_config_keys);
            Ok(())
        }
    }
}

/// `nulang nula auth enable` — enable the @nulang/auth module for the
/// current package. Currently this prints the required setup steps; in the
/// future it will also scaffold the session actor and update the manifest.
fn run_auth_enable(_args: &[String]) -> NuResult<()> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);

    let already_depends = if manifest_path.exists() {
        let content =
            std::fs::read_to_string(&manifest_path).map_err(|e| NuError::PackageError {
                msg: format!("cannot read {}: {}", manifest_path.display(), e),
                span: Span::default(),
            })?;
        content.contains("nulang-auth")
    } else {
        false
    };

    if !already_depends {
        println!("Add @nulang/auth to your Nulang.toml [dependencies], for example:");
        println!("  nulang-auth = {{ path = \"packages/nulang-auth\" }}");
    } else {
        println!("@nulang/auth is already a dependency.");
    }
    println!("Set the cloud config key AUTH_COOKIE_SECRET on deploy.");
    Ok(())
}

/// `nula new <name> [--template <name>]`: scaffold a package directory.
fn cmd_new(path_arg: Option<&str>, template: Option<&str>) -> NuResult<()> {
    let path_str = path_arg.ok_or_else(|| NuError::PackageError {
        msg: "nula new requires a package name or path".to_string(),
        span: Span::default(),
    })?;
    let dir = PathBuf::from(path_str);
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| NuError::PackageError {
            msg: format!("invalid path '{}' — cannot extract package name", path_str),
            span: Span::default(),
        })?;
    validate_package_name(name)?;
    if dir.exists() {
        return Err(NuError::PackageError {
            msg: format!("directory '{}' already exists", dir.display()),
            span: Span::default(),
        });
    }
    let tmpl = template.unwrap_or("default");
    let valid = [
        "default",
        "cli",
        "lib",
        "full",
        "distributed",
        "ai-agent",
        "web",
    ];
    if !valid.contains(&tmpl) {
        return Err(NuError::PackageError {
            msg: format!(
                "unknown template '{}' (available: {})",
                tmpl,
                valid.join(", ")
            ),
            span: Span::default(),
        });
    }
    scaffold_package(&dir, name, tmpl)?;
    println!("Created package '{}' at '{}'", name, dir.display());
    Ok(())
}

/// `nula init`: scaffold a package in the current directory.
fn cmd_init() -> NuResult<()> {
    let dir = package_root()?;
    let manifest_path = dir.join(MANIFEST_FILE);
    if manifest_path.exists() {
        return Err(NuError::PackageError {
            msg: format!(
                "{} already exists in {} — package is already initialized",
                MANIFEST_FILE,
                dir.display()
            ),
            span: Span::default(),
        });
    }
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("nulang-project");
    validate_package_name(name)?;
    scaffold_package(&dir, name, "default")?;
    // Write a basic .gitignore
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::write(&gitignore, "# Nulang build artifacts\n*.nbc\n.nula/\n");
    }
    println!("Initialized package '{}' in '{}'", name, dir.display());
    Ok(())
}

fn validate_package_name(name: &str) -> NuResult<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(NuError::PackageError {
            msg: format!(
                "invalid package name '{}' (use letters, digits, '-' or '_')",
                name
            ),
            span: Span::default(),
        });
    }
    Ok(())
}

/// Write the `Nulang.toml` + template source files for a new package.
fn scaffold_package(dir: &Path, name: &str, template: &str) -> NuResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", dir.display(), e),
        span: Span::default(),
    })?;
    let manifest_path = dir.join(MANIFEST_FILE);
    std::fs::write(
        &manifest_path,
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
            name
        ),
    )
    .map_err(|e| NuError::PackageError {
        msg: format!("cannot write {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;
    if template == "web" {
        let mut web_manifest =
            std::fs::read_to_string(&manifest_path).map_err(|e| NuError::PackageError {
                msg: format!("cannot read {}: {}", manifest_path.display(), e),
                span: Span::default(),
            })?;
        web_manifest
            .push_str("[web]\nport = 8787\nstatic_dir = \"public\"\noutput_dir = \"dist\"\n");
        std::fs::write(&manifest_path, web_manifest).map_err(|e| NuError::PackageError {
            msg: format!(
                "cannot write web section to {}: {}",
                manifest_path.display(),
                e
            ),
            span: Span::default(),
        })?;
    }

    for (rel_path, content) in template_files(template) {
        let dest = dir.join(rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NuError::PackageError {
                msg: format!("cannot create {}: {}", parent.display(), e),
                span: Span::default(),
            })?;
        }
        std::fs::write(&dest, content).map_err(|e| NuError::PackageError {
            msg: format!("cannot write {}: {}", dest.display(), e),
            span: Span::default(),
        })?;
    }
    Ok(())
}

/// Return the list of (relative_path, content) for a named template.
fn template_files(name: &str) -> Vec<(&'static str, &'static str)> {
    match name {
        "default" => vec![(
            "src/main.nula",
            "fn main() {\n  perform IO.print(\"Hello from Nulang!\")\n}\n",
        )],
        "cli" => vec![(
            "src/main.nula",
            "// CLI template — a starting point for command-line tools.\n//\n// This file demonstrates:\n//   System.arg   — reading command-line arguments (0-indexed)\n//   Env.get      — reading environment variables\n//   FS.write     — writing output to a file\n//   IO.print     — printing to stdout\n//   == nil       — checking whether a value is nil\n\n// ── Helper (defined before `main` so it is in scope) ───────────────────────\n// Print a friendly greeting.  Extracted as a function so the main flow\n// stays readable.\nfn greet(name) {\n  perform IO.print(\"Hello, \" + name + \"!\")\n}\n\n// ── Entry point ────────────────────────────────────────────────────────────\n\nfn main() {\n  // ── 1. Read the target name from the command line ──────────────────────\n  // System.arg(0) = program name; System.arg(1) = script path.\n  // System.arg(2) is the first user argument.\n  let given = perform System.arg(2)\n\n  // ── 2. Fall back to the USER environment variable ─────────────────────\n  // Env.get returns nil when the variable is not set.\n  match given {\n    nil => {\n      let user = perform Env.get(\"USER\")\n      match user {\n        nil => {\n          // ── 3. Hard-coded default when nothing else is available ──────\n          // Both arg and env were nil — use \"World\" as a friendly fallback.\n          let name = \"World\"\n          // Print usage hint because the user didn't supply a name.\n          let prog = perform System.arg(0)\n          perform IO.print(\"Usage: \" + prog + \" <name>\")\n          perform IO.print(\"\")\n          greet(name)\n        }\n        _ => {\n          // Env.get returned a value; use it.\n          greet(user)\n        }\n      }\n    }\n    _ => {\n      // System.arg(2) returned a value; use it.\n      greet(given)\n    }\n  }\n\n  // ── 4. Optional: log the greeting to a file ───────────────────────────\n  // Pass --log <path> to write the greeting to a file.\n  // This shows how to handle optional flags and do file output.\n  let log_path = perform System.arg(3)\n  match log_path {\n    nil => unit,\n    _ => {\n      match log_path {\n        \"--log\" => {\n          let path = perform System.arg(4)\n          match path {\n            nil => {\n              perform IO.print(\"--log requires a file path\")\n            }\n            _ => {\n              // Build the log line with timestamp-like prefix.\n              let ts = perform Env.get(\"NU_TIMESTAMP\")\n              let prefix = match ts {\n                nil => \"[nulang]\",\n                _   => \"[\" + ts + \"]\"\n              }\n              let out_name = match given {\n                nil => match perform Env.get(\"USER\") {\n                  nil => \"World\",\n                  u   => u\n                },\n                n   => n\n              }\n              let line = prefix + \" Greeted \" + out_name\n              let wrote = perform FS.write(path, line)\n              match wrote {\n                nil => perform IO.print(\"Warning: could not write log to \" + path),\n                _   => perform IO.print(\"Logged to \" + path)\n              }\n            }\n          }\n        }\n        _ => unit\n      }\n    }\n  }\n}\n",
        )],
        "lib" => vec![
            (
                "src/main.nula",
                "// Entry point for a library package.\n//\n// Library packages export public functions from `src/lib.nula` for other\n// packages to depend on. The entry point is a trivial smoke test — replace\n// it with your own application logic.\n\nfn main() {\n  perform IO.print(\"Library package ready.\")\n  perform IO.print(\"Run `nula test` to verify the public API.\")\n}\n",
            ),
            (
                "src/lib.nula",
                "/// Add two integers and return the sum.\npub fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n",
            ),
            (
                "tests/test_add.nula",
                "// Test file for the library's `add` function.\n//\n// Each test file runs standalone via `nula test` — helper functions must\n// be defined in the test file itself (or imported when the module system\n// supports cross-file imports).\n\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\nfn main() {\n  perform Test.assert_eq(add(1, 2), 3)\n  perform Test.assert_eq(add(-5, 5), 0)\n}\n",
            ),
        ],
        "full" => vec![
            (
                "README.md",
                "# {{name}}\n\nA Nulang project.\n\n## Structure\n\n- `src/main.nula`   — entry point\n- `src/lib.nula`     — library module\n- `tests/`           — test files\n- `examples/`        — standalone demos\n\n## Commands\n\n```\n# Build and type-check\nnula build\n\n# Run the entry point\nnula run\n\n# Run tests\nnula test\n\n# Run a demo\nnulang examples/demo.nula\n```\n\n## Dependencies\n\nAdd dependencies with `nula add <name>`.\n",
            ),
            (
                "src/lib.nula",
                "// Library module — reusable functions shared across the project.\n//\n// Public functions (marked `pub`) can be imported by other files.\n// Use `///` doc comments to document public API surfaces.\n\n/// Return a greeting for the given name.\npub fn greet(name: String) -> String {\n  \"Hello, \" + name + \"!\"\n}\n\n/// Add two integers together.\npub fn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n/// Compute the factorial of n recursively.\npub fn factorial(n: Int) -> Int {\n  if n <= 1 then 1\n  else n * factorial(n - 1)\n}\n\n/// Return a friendly message describing the sign of a number.\npub fn describe_number(n: Int) -> String {\n  if n > 0 then \"positive\"\n  else if n < 0 then \"negative\"\n  else \"zero\"\n}\n",
            ),
            (
                "src/main.nula",
                "// Entry point for the application.\n// All application logic lives here; the build system type-checks this file.\n\n// ── Library functions (defined before `main` so they are in scope) ─────────\n\n/// Return a greeting for the given name.\nfn greet(name: String) -> String {\n  \"Hello, \" + name + \"!\"\n}\n\n/// Return a friendly label for a number's sign.\nfn describe_number(n: Int) -> String {\n  if n > 0 then \"positive\"\n  else if n < 0 then \"negative\"\n  else \"zero\"\n}\n\n// ── Entry point ────────────────────────────────────────────────────────────\n\nfn main() {\n  // Read an optional count from the environment.\n  let upto = perform Env.get(\"COUNT\")\n  let n = match upto {\n    nil => 10,\n    _   => perform Int.parse(upto)\n  }\n\n  let msg = greet(\"Nulang\")\n  perform IO.print(msg)\n\n  // Demonstrate basic operations.\n  let sum = n + 42\n  perform IO.print(\"n + 42 = \" + perform Int.to_string(sum))\n\n  let desc = describe_number(n)\n  perform IO.print(\"n is \" + desc)\n}\n",
            ),
            (
                "tests/test_lib.nula",
                "// Tests for the library functions.\n//\n// Each test file runs standalone via `nula test`.\n// Define helper functions before `main` so they are in scope.\n\n/// Return a greeting for the given name.\nfn greet(name: String) -> String {\n  \"Hello, \" + name + \"!\"\n}\n\n/// Add two integers together.\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n/// Compute the factorial of n recursively.\nfn factorial(n: Int) -> Int {\n  if n <= 1 then 1\n  else n * factorial(n - 1)\n}\n\n/// Return a friendly message describing the sign of a number.\nfn describe_number(n: Int) -> String {\n  if n > 0 then \"positive\"\n  else if n < 0 then \"negative\"\n  else \"zero\"\n}\n\nfn main() {\n  perform Test.assert_eq(greet(\"World\"), \"Hello, World!\")\n  perform Test.assert_eq(add(40, 2), 42)\n  perform Test.assert_eq(factorial(5), 120)\n  perform Test.assert_eq(describe_number(7), \"positive\")\n  perform Test.assert_eq(describe_number(-3), \"negative\")\n  perform Test.assert_eq(describe_number(0), \"zero\")\n}\n",
            ),
            (
                "examples/demo.nula",
                "// Demo script — a small standalone example using the library.\n//\n// Run with:  nulang examples/demo.nula\n\n/// Return a greeting for the given name.\nfn greet(name: String) -> String {\n  \"Hello, \" + name + \"!\"\n}\n\n/// Add two integers together.\nfn add(a: Int, b: Int) -> Int {\n  a + b\n}\n\n/// Compute the factorial of n recursively.\nfn factorial(n: Int) -> Int {\n  if n <= 1 then 1\n  else n * factorial(n - 1)\n}\n\n/// Return a friendly message describing the sign of a number.\nfn describe_number(n: Int) -> String {\n  if n > 0 then \"positive\"\n  else if n < 0 then \"negative\"\n  else \"zero\"\n}\n\nfn main() {\n  let msg = greet(\"demo user\")\n  perform IO.print(msg)\n\n  let f = factorial(6)\n  perform IO.print(\"6! = \" + perform Int.to_string(f))\n\n  let d = describe_number(42)\n  perform IO.print(\"42 is \" + d)\n\n  let s = add(100, 200)\n  perform IO.print(\"100 + 200 = \" + perform Int.to_string(s))\n}\n",
            ),
        ],
        "distributed" => vec![
            (
                "src/main.nula",
                "// Distributed template — supervised, message-passing worker actors.\n//\n// Demonstrates: actor declaration, `spawn Actor {}`, message passing\n// with `!`, durable state per actor, and an OTP supervisor that\n// restarts a worker on abnormal exit.\n//\n// Run with: nula run\n\n// A worker actor. `count` is per-actor state mutated by the `work`\n// behavior; each spawned worker has its own independent copy.\nactor Worker {\n    state count: Int = 0\n\n    behavior work(by: Int) {\n        self.count = self.count + by\n    }\n\n    behavior report() {\n        perform IO.print(\"  worker count=\" + perform Int.to_string(self.count))\n    }\n}\n\nfn main() {\n    // Spawn two independent workers and route work between them.\n    let w1 = spawn Worker {}\n    let w2 = spawn Worker {}\n\n    w1 ! work(10)\n    w1 ! work(5)\n    w2 ! work(7)\n\n    w1 ! report()\n    w2 ! report()\n\n    perform IO.print(\"Two distributed workers are running.\")\n}\n",
            ),
        ],
        "ai-agent" => vec![
            (
                "src/main.nula",
                "// AI-agent template — an actor with conversation memory backed by the\n// `Inference.ask` effect (LLM). Demonstrates actor state, behaviors,\n// and a non-blocking inference call.\n//\n// Requires the `ai-runtime` cargo feature (enabled by default).\n//\n// Run with: nula run\n\nactor ChatAgent {\n    state history: String = \"\"\n    state turn: Int = 0\n\n    behavior ask(prompt: String) {\n        self.turn = self.turn + 1\n        let reply = perform Inference.ask(prompt)\n        self.history = self.history + \"\\nQ: \" + prompt + \"\\nA: \" + reply\n        perform IO.print(\"[Turn \" + perform Int.to_string(self.turn) + \"] \" + reply)\n    }\n\n    behavior summary() {\n        let s = perform Inference.ask(\n            \"Summarize this conversation:\\n\" + self.history\n        )\n        perform IO.print(\"Summary: \" + s)\n    }\n}\n\nfn main() {\n    let chat = spawn ChatAgent {}\n    chat ! ask(\"Hello! Introduce yourself in one sentence.\")\n    chat ! summary()\n    perform IO.print(\"Agent ready. Configure your provider in Nulang.toml.\")\n}\n",
            ),
        ],
        "web" => vec![
            (
                "src/main.nula",
                r#"// Nulang Web app — a compiler-first full-stack Todo list.
//
// Run with: `nula dev` (development server) or `nula build --web` (SSG).
//
// The server renders the initial HTML and handles form POSTs with an
// ephemeral in-memory store. The generated `app.client.js` intercepts
// form submissions so the page updates without a full reload when JS is
// enabled.

import stdlib::web::html
import stdlib::web::types
import stdlib::web::host
import stdlib::json

fn get_todos() -> [JsonValue] {
    let raw = kv_get("todos")
    if raw == "" then { [] }
    else {
        match parse(raw) {
            JsonArray(items) => items
            _ => []
        }
    }
}

fn set_todos(todos) {
    kv_set("todos", stringify(JsonArray(todos)))
}

fn next_id() -> Int {
    let raw = kv_get("todo_counter")
    let n = if raw == "" then { 0 } else { perform String.to_int(raw) }
    kv_set("todo_counter", perform Int.to_string(n + 1))
    n + 1
}

fn add_todo(title) {
    let id = next_id()
    let todos = get_todos()
    let todo = JsonObject([
        ("id", JsonNumber(perform Int.to_float(id))),
        ("title", JsonString(title)),
        ("done", JsonBool(false))
    ])
    set_todos(perform Array.push(todos, todo))
}

fn toggle_todo(id_str) {
    let id = perform String.to_int(id_str)
    let todos = get_todos()
    var updated = []
    for todo in todos {
        let tid = perform Float.to_int(get_number(todo, "id", 0.0))
        if tid == id then {
            let title = get_string(todo, "title", "")
            let done = get_bool(todo, "done", false)
            updated = perform Array.push(updated, JsonObject([
                ("id", JsonNumber(perform Int.to_float(tid))),
                ("title", JsonString(title)),
                ("done", JsonBool(!done))
            ]))
        } else {
            updated = perform Array.push(updated, todo)
        }
    }
    set_todos(updated)
}

fn delete_todo(id_str) {
    let id = perform String.to_int(id_str)
    let todos = get_todos()
    var updated = []
    for todo in todos {
        let tid = perform Float.to_int(get_number(todo, "id", 0.0))
        if tid != id then {
            updated = perform Array.push(updated, todo)
        } else {}
    }
    set_todos(updated)
}

fn render_todos() -> Html {
    let todos = get_todos()
    var rows = []
    for todo in todos {
        let id = perform Float.to_int(get_number(todo, "id", 0.0))
        let id_str = perform Int.to_string(id)
        let title = get_string(todo, "title", "")
        let done = get_bool(todo, "done", false)
        let label = if done then perform String.concat(title, " (done)") else title
        let row = el("li", [("class", if done then text("done") else text(""))], [
            el("span", [], [text(label)]),
            el("form", [("action", text("/")), ("data-action", text("toggle_todo")), ("data-action-placement", text("server")), ("method", text("POST"))], [
                el("input", [("type", text("hidden")), ("name", text("id")), ("value", text(id_str))], []),
                el("button", [("type", text("submit"))], [text(if done then "Undo" else "Done")])
            ]),
            el("form", [("action", text("/")), ("data-action", text("delete_todo")), ("data-action-placement", text("server")), ("method", text("POST"))], [
                el("input", [("type", text("hidden")), ("name", text("id")), ("value", text(id_str))], []),
                el("button", [("type", text("submit"))], [text("Delete")])
            ])
        ])
        rows = perform Array.push(rows, row)
    }
    el("ul", [("class", text("todos"))], rows)
}

fn home() -> Html {
    if request_method() == "POST" then {
        let action = form_value("__nulang_action")
        if action == "add_todo" then {
            let title = form_value("title")
            if title != "" then { add_todo(title) } else {}
        } else if action == "toggle_todo" then {
            toggle_todo(form_value("id"))
        } else if action == "delete_todo" then {
            delete_todo(form_value("id"))
        } else {}
    } else {}
    document(
        head([
            title("Nulang Todo"),
            el("link", [("rel", text("stylesheet")), ("href", text("style.css"))], [])
        ]),
        body([
            el("h1", [], [text("Nulang Todo")]),
            el("form", [("action", text("/")), ("data-action", text("add_todo")), ("data-action-placement", text("server")), ("method", text("POST"))], [
                el("input", [("type", text("text")), ("name", text("title")), ("placeholder", text("New todo..."))], []),
                el("button", [("type", text("submit"))], [text("Add")])
            ]),
            render_todos()
        ])
    )
}

app "todos" {
    route "GET" "/" -> home
    route "POST" "/" -> home
}
"#,
            ),
            (
                "public/style.css",
                r#"body {
  font-family: system-ui, sans-serif;
  max-width: 40rem;
  margin: 2rem auto;
  padding: 0 1rem;
  line-height: 1.5;
}

h1 { color: #1a1a1a; }

form {
  display: flex;
  gap: 0.5rem;
  margin-bottom: 1rem;
}

input[type="text"] {
  flex: 1;
  padding: 0.4rem;
}

button {
  padding: 0.4rem 0.8rem;
  cursor: pointer;
}

.todos {
  list-style: none;
  padding: 0;
}

.todos li {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.5rem;
  border-bottom: 1px solid #ddd;
}

.todos li.done span {
  text-decoration: line-through;
  color: #666;
}

.todos li form {
  margin: 0;
}
"#,
            ),
        ],
        _ => unreachable!(),
    }
}

/// Resolve the package in the current directory, write `Nulang.lock`, and
/// return the entry point path.
fn prepare_package() -> NuResult<PathBuf> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!(
            "failed to load {} at {}: {}",
            MANIFEST_FILE,
            root.display(),
            e
        ),
        span: Span::default(),
    })?;

    if let Some(req) = &manifest.package.language {
        use crate::format::constants::LANGUAGE_VERSION_STR;

        let parse_maj_min = |s: &str| -> Option<(u32, u32)> {
            let s = s.split('-').next().unwrap_or(s);
            let mut parts = s.split('.');
            let maj = parts.next()?.parse().ok()?;
            let min = parts.next()?.parse().ok()?;
            Some((maj, min))
        };

        if let (Some((req_maj, req_min)), Some((tool_maj, tool_min))) =
            (parse_maj_min(req), parse_maj_min(LANGUAGE_VERSION_STR))
        {
            if req_maj != tool_maj || req_min != tool_min {
                return Err(NuError::PackageError {
                    msg: format!(
                        "package requires language {} but this toolchain provides {}",
                        req, LANGUAGE_VERSION_STR
                    ),
                    span: Span::default(),
                });
            }
        } else {
            // fallback exact match if parsing fails
            let req_base = req.split('-').next().unwrap_or(req);
            let tool_base = LANGUAGE_VERSION_STR
                .split('-')
                .next()
                .unwrap_or(LANGUAGE_VERSION_STR);
            if req_base != tool_base {
                return Err(NuError::PackageError {
                    msg: format!(
                        "package requires language {} but this toolchain provides {}",
                        req, LANGUAGE_VERSION_STR
                    ),
                    span: Span::default(),
                });
            }
        }
    }

    eprintln!("  Resolving dependencies...");
    let resolution = resolve(&root, &manifest).map_err(|e| NuError::PackageError {
        msg: format!(
            "failed to resolve dependencies for package '{}': {}\n  help: check that all [dependencies] in {} are reachable",
            manifest.package.name,
            e,
            manifest_path.display()
        ),
        span: Span::default(),
    })?;

    let lock_path = root.join(LOCKFILE_FILE);
    resolution
        .to_lockfile()
        .save(&root)
        .map_err(|e| NuError::PackageError {
            msg: format!("failed to write {}: {}", lock_path.display(), e),
            span: Span::default(),
        })?;

    let entry = root.join(&manifest.package.entry);
    if !entry.exists() {
        return Err(NuError::PackageError {
            msg: format!(
                "entry point '{}' not found (defined as `entry = \"{}\"` in {})",
                entry.display(),
                manifest.package.entry,
                manifest_path.display()
            ),
            span: Span::default(),
        });
    }
    Ok(entry)
}

/// `--with <cap>` argument pairs for the current package's declared
/// `[package] capabilities` (empty when none are declared or the manifest
/// can't be loaded). Lets packages that perform gated resource effects
/// (e.g. `Http` → `net`) pass the default-deny capability check by
/// declaring their requirements in `Nulang.toml`.
fn capability_args() -> Vec<String> {
    let root = match package_root() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    match Manifest::load(&root) {
        Ok(m) => m
            .package
            .capabilities
            .iter()
            .flat_map(|c| ["--with".to_string(), c.clone()])
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Run the current `nulang` executable with `args`, inheriting stdio.
fn nulang_exe(args: &[&str]) -> NuResult<()> {
    // When running inside `cargo test`, the current executable is the test
    // harness, not the CLI binary. `CARGO_BIN_EXE_nulang` points to the real
    // binary when cargo builds it alongside integration tests; for unit tests
    // we fall back to `target/<profile>/nulang` next to the deps directory.
    let current_exe = std::env::current_exe().ok();
    let exe = std::env::var_os("CARGO_BIN_EXE_nulang")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            current_exe.as_ref().and_then(|p| {
                p.parent()
                    .and_then(|deps| deps.parent())
                    .map(|profile| profile.join("nulang"))
                    .filter(|candidate| candidate.is_file())
            })
        })
        .or_else(|| {
            // Coverage runs (`cargo llvm-cov`) use a separate target dir
            // that has no standalone binary next to the test harness.
            // Resolve the repo's configured target dir via cargo metadata
            // (same approach as conformance/run.py) and use its debug bin.
            let out = std::process::Command::new("cargo")
                .args(["metadata", "--format-version", "1", "--no-deps"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let meta: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
            let td = meta.get("target_directory")?.as_str()?;
            let candidate = std::path::Path::new(td).join("debug").join("nulang");
            candidate.is_file().then_some(candidate)
        })
        .or_else(|| current_exe.clone())
        .ok_or_else(|| NuError::PackageError {
            msg: "cannot locate nulang executable".to_string(),
            span: Span::default(),
        })?;
    let mut cmd = Command::new(&exe);
    cmd.args(args);
    // Auto-detect the stdlib directory relative to the executable so
    // that `import stdlib::*` works without setting NULANG_STDLIB.
    if std::env::var_os("NULANG_STDLIB").is_none() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("stdlib");
            if candidate.is_dir() {
                cmd.env("NULANG_STDLIB", &candidate);
            } else {
                // Development fallback: when running the freshly-built `nula`
                // binary from the source tree, stdlib lives at src/stdlib/.
                let dev = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("src")
                    .join("stdlib");
                if dev.is_dir() {
                    cmd.env("NULANG_STDLIB", &dev);
                }
            }
        }
    }

    // Build a module path from the current lockfile so that
    // `import @nulang/foo` resolves to the dependency's source directory.
    let mut module_path = std::env::var("NULANG_MODULE_PATH").unwrap_or_default();
    if let Some(computed) = build_module_path() {
        if !module_path.is_empty() {
            module_path.push(';');
        }
        module_path.push_str(&computed);
    }
    if !module_path.is_empty() {
        cmd.env("NULANG_MODULE_PATH", &module_path);
    }

    let status = cmd.status().map_err(|e| NuError::PackageError {
        msg: format!("failed to run nulang ({}): {}", exe.display(), e),
        span: Span::default(),
    })?;
    if !status.success() {
        return Err(NuError::PackageError {
            msg: format!("nulang {} exited with {}", args.join(" "), status),
            span: Span::default(),
        });
    }
    Ok(())
}

/// Build a NULANG_MODULE_PATH string from the current lockfile, mapping each
/// resolved dependency's source directory to an `@nulang/<name>` import.
fn build_module_path() -> Option<String> {
    let root = package_root().ok()?;
    let lockfile = Lockfile::load(&root).ok()?;
    let mut entries = Vec::new();
    for pkg in &lockfile.package {
        let src_dir = match pkg.source.as_str() {
            s if s.starts_with("path+") => std::path::PathBuf::from(&s[5..]).join("src"),
            s if s.starts_with("git+") => {
                root.join(".nula").join("git").join(&pkg.name).join("src")
            }
            s if s.starts_with("reg+") => root
                .join(".nula")
                .join("registry")
                .join(format!("{}-{}", pkg.name, pkg.version))
                .join("src"),
            _ => continue,
        };
        if src_dir.is_dir() {
            let import_name = if pkg.name.starts_with("nulang-") {
                &pkg.name["nulang-".len()..]
            } else {
                &pkg.name[..]
            };
            entries.push(format!("@nulang/{}={}", import_name, src_dir.display()));
            if import_name != pkg.name {
                entries.push(format!("@nulang/{}={}", pkg.name, src_dir.display()));
            }
        }
    }
    if entries.is_empty() {
        None
    } else {
        Some(entries.join(";"))
    }
}

/// `nula build`: resolve dependencies, write the lockfile, type-check entry.
/// `nula build`: resolve dependencies, write the lockfile, type-check and
/// compile to a .nbc artifact in .nula/dist/.
fn cmd_build(json: bool) -> NuResult<()> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;
    let name = manifest.package.name.clone();

    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();

    if json {
        return cmd_build_json(&root, &name, &entry_str);
    }

    let dist_dir = root.join(".nula").join("dist");
    std::fs::create_dir_all(&dist_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", dist_dir.display(), e),
        span: Span::default(),
    })?;

    let nbc_path = dist_dir.join(format!("{}.nbc", name));
    let nbc_path_str = nbc_path.to_string_lossy().into_owned();

    eprintln!("Building {}...", name);
    eprintln!("  Type-checking {}...", entry.display());
    let caps = capability_args();
    let cap_refs: Vec<&str> = caps.iter().map(|s| s.as_str()).collect();
    nulang_exe(&[&["--check", &entry_str], &cap_refs[..]].concat())?;
    eprintln!("  Compiling {} to .nbc...", name);
    nulang_exe(
        &[
            &["--emit-nbc", "--out", &nbc_path_str, &entry_str],
            &cap_refs[..],
        ]
        .concat(),
    )?;
    println!("Build succeeded.");
    Ok(())
}

/// `nula build --json`: machine-readable build report. The JSON report is
/// the only output on stdout; progress stays on stderr. Type-check
/// diagnostics are produced by the child `nulang --check --json` invocation
/// and forwarded (re-wrapped as `command: "build"`) so consumers see one
/// schema.
fn cmd_build_json(root: &Path, name: &str, entry_str: &str) -> NuResult<()> {
    use crate::json_diagnostics::{diagnostic_from_message, JsonReport, SCHEMA_VERSION};

    // Step 1: type-check with JSON diagnostics, capturing the child's stdout.
    eprintln!("Building {}...", name);
    eprintln!("  Type-checking {}...", entry_str);
    let check = nulang_exe_output(&["--json", "--check", entry_str])?;
    if !check.status.success() {
        let stdout = String::from_utf8_lossy(&check.stdout);
        // Forward the child's structured diagnostics when parseable; fall
        // back to a single opaque error otherwise.
        let report = match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
            Ok(v) if v["diagnostics"].is_array() => serde_json::json!({
                "schema_version": SCHEMA_VERSION,
                "command": "build",
                "file": entry_str,
                "ok": false,
                "diagnostics": v["diagnostics"],
            })
            .to_string(),
            _ => JsonReport::new(
                "build",
                Some(entry_str.to_string()),
                vec![diagnostic_from_message(format!(
                    "nulang --check exited with {}",
                    check.status
                ))],
            )
            .to_json_string(),
        };
        println!("{}", report.trim_end());
        return Err(NuError::PackageError {
            msg: format!("type check failed for {}", entry_str),
            span: Span::default(),
        });
    }

    // Step 2: compile to .nbc (stdout captured so only JSON reaches stdout).
    let dist_dir = root.join(".nula").join("dist");
    std::fs::create_dir_all(&dist_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", dist_dir.display(), e),
        span: Span::default(),
    })?;
    let nbc_path = dist_dir.join(format!("{}.nbc", name));
    let nbc_path_str = nbc_path.to_string_lossy().into_owned();
    eprintln!("  Compiling {} to .nbc...", name);
    let compile = nulang_exe_output(&["--emit-nbc", "--out", &nbc_path_str, entry_str])?;
    if !compile.status.success() {
        let stderr = String::from_utf8_lossy(&compile.stderr).trim().to_string();
        let report = JsonReport::new(
            "build",
            Some(entry_str.to_string()),
            vec![diagnostic_from_message(format!(
                "nulang --emit-nbc exited with {}: {}",
                compile.status, stderr
            ))],
        );
        print!("{}", report.to_json_string());
        return Err(NuError::PackageError {
            msg: format!("compilation failed for {}", entry_str),
            span: Span::default(),
        });
    }

    let report = JsonReport::new("build", Some(entry_str.to_string()), Vec::new());
    print!("{}", report.to_json_string());
    Ok(())
}

/// Run the nulang exe with piped stdout/stderr, returning the full output.
/// Used by `--json` modes where child output must not reach our stdout.
fn nulang_exe_output(args: &[&str]) -> NuResult<std::process::Output> {
    let exe = std::env::current_exe().map_err(|e| NuError::PackageError {
        msg: format!("cannot locate nulang executable: {}", e),
        span: Span::default(),
    })?;
    let mut cmd = Command::new(&exe);
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if std::env::var_os("NULANG_STDLIB").is_none() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("stdlib");
            if candidate.is_dir() {
                cmd.env("NULANG_STDLIB", &candidate);
            }
        }
    }
    cmd.output().map_err(|e| NuError::PackageError {
        msg: format!("failed to run nulang ({}): {}", exe.display(), e),
        span: Span::default(),
    })
}

/// `nula build-wasm`: compile package to .wasm + AOT .cwasm.
/// `nula build-wasm`: compile package to .wasm + AOT .cwasm in .nula/dist/.
fn cmd_build_wasm() -> NuResult<()> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;
    let name = manifest.package.name.clone();

    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();

    let dist_dir = root.join(".nula").join("dist");
    std::fs::create_dir_all(&dist_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", dist_dir.display(), e),
        span: Span::default(),
    })?;

    let wasm_path = dist_dir.join(format!("{}.wasm", name));
    let wasm_path_str = wasm_path.to_string_lossy().into_owned();

    eprintln!("Building {} (WASM AOT)...", name);
    eprintln!("  Compiling {} to WASM...", entry.display());
    nulang_exe(&["--backend", "wasm-aot", "--out", &wasm_path_str, &entry_str])?;
    println!("WASM AOT build succeeded.");
    Ok(())
}

/// `nula run`: build, then execute the entry point.
fn cmd_run() -> NuResult<()> {
    eprintln!("Building and running...");
    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();
    let caps = capability_args();
    let cap_refs: Vec<&str> = caps.iter().map(|s| s.as_str()).collect();
    nulang_exe(&[&[entry_str.as_str()], &cap_refs[..]].concat())
}

/// `nula run --watch` (or `nula watch`): build, run, and re-run when source
/// files change under `src/`. Uses simple mtime polling.
fn cmd_run_watch() -> NuResult<()> {
    let root = package_root()?;
    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();

    // Initial run
    eprintln!("Building and running...");
    nulang_exe(&[&entry_str])?;

    // Collect initial mtimes for all .nula files under src/
    let src_dir = root.join("src");
    let mut last_mtimes = collect_mtimes(&src_dir);

    println!("watching... (Ctrl-C to stop)");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let current = collect_mtimes(&src_dir);
        if current != last_mtimes {
            last_mtimes = current;
            eprintln!("\n--- change detected, rebuilding ---");
            // Re-resolve in case dependencies changed
            match prepare_package() {
                Ok(entry) => {
                    let es = entry.to_string_lossy().into_owned();
                    let _ = nulang_exe(&[&es]);
                }
                Err(e) => eprintln!("Error: {}", e),
            }
        }
    }
}

/// Collect (path, mtime) pairs for all .nula files under `dir`, sorted by path.
fn collect_mtimes(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut result = Vec::new();
    collect_mtimes_recursive(dir, &mut result);
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn collect_mtimes_recursive(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_mtimes_recursive(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "nula") {
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(mtime) = meta.modified() {
                    out.push((path, mtime));
                }
            }
        }
    }
}

/// `nula build --web`: static-site build. Compile the entry point, run it to
/// collect `Web.route` registrations, render each static handler to an HTML
/// file, and copy the static directory into the output directory.
fn cmd_build_web() -> NuResult<()> {
    let root = package_root()?;
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", MANIFEST_FILE, e),
        span: Span::default(),
    })?;
    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();

    let build_dir = root.join(".nula");
    std::fs::create_dir_all(&build_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", build_dir.display(), e),
        span: Span::default(),
    })?;
    let nbc_path = build_dir.join("build.nbc");
    let nbc_path_str = nbc_path.to_string_lossy().into_owned();

    let output_dir = root.join(&manifest.web.output_dir);
    std::fs::create_dir_all(&output_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", output_dir.display(), e),
        span: Span::default(),
    })?;

    let client_js_path = output_dir.join("app.client.js");
    let client_js_path_str = client_js_path.to_string_lossy().into_owned();

    eprintln!("Building {} (web)...", manifest.package.name);
    eprintln!("  Compiling {}...", entry.display());
    nulang_exe(&[
        "--emit-nbc",
        "--out",
        &nbc_path_str,
        "--rewrite-signals",
        &client_js_path_str,
        &entry_str,
    ])?;

    let bytes = std::fs::read(&nbc_path).map_err(|e| NuError::PackageError {
        msg: format!("cannot read {}: {}", nbc_path.display(), e),
        span: Span::default(),
    })?;
    let artifact = CodeModule::from_nbc(&bytes).map_err(|e| NuError::PackageError {
        msg: format!("cannot decode {}: {}", nbc_path.display(), e),
        span: Span::default(),
    })?;

    let mut vm = VM::new();
    vm.load_module(artifact.module);
    vm.run().map_err(|e| NuError::PackageError {
        msg: format!("runtime error in {}: {}", entry.display(), e),
        span: Span::default(),
    })?;
    let routes = vm.take_web_routes();

    eprintln!("  Rendering {} route(s)...", routes.len());
    for route in &routes {
        let html = render_route_handler(&route.handler_module, route.handler_func_idx, None)
            .ok_or_else(|| NuError::PackageError {
                msg: format!("failed to render route {:?} {}", route.method, route.path),
                span: Span::default(),
            })?;
        let html = crate::web::reactivity::inject_client_runtime_script(&html);
        let out_file = route_path_to_output_file(&route.path);
        let dest = output_dir.join(&out_file);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| NuError::PackageError {
                msg: format!("cannot create {}: {}", parent.display(), e),
                span: Span::default(),
            })?;
        }
        std::fs::write(&dest, html).map_err(|e| NuError::PackageError {
            msg: format!("cannot write {}: {}", dest.display(), e),
            span: Span::default(),
        })?;
        eprintln!("    {} -> {}", route.path, dest.display());
    }

    let static_dir = root.join(&manifest.web.static_dir);
    if static_dir.is_dir() {
        copy_dir_contents(&static_dir, &output_dir)?;
    }

    // Emit compile-time signal graph if the entry uses `signal` declarations.
    let signals_path = output_dir.join("app.signals.json");
    nulang_exe(&[
        "--emit-signals",
        &signals_path.to_string_lossy(),
        &entry_str,
    ])?;

    // Generate deployment IR consumed by adapters and Nulang Cloud.
    let src_root = root.join("src");
    let ir = crate::web::ir::generate_deployment_ir(
        &routes,
        Some(&signals_path),
        &src_root,
        &manifest.budgets,
    );
    let ir_path = output_dir.join("nulang-app.ir.json");
    let ir_json = ir.to_json();
    std::fs::write(&ir_path, ir_json).map_err(|e| NuError::PackageError {
        msg: format!("cannot write {}: {}", ir_path.display(), e),
        span: Span::default(),
    })?;
    eprintln!("  Wrote {}", ir_path.display());

    // Enforce performance budgets declared in Nulang.toml.
    if let Err(violations) = crate::web::budget::check_initial_js_budget(
        &output_dir,
        manifest.budgets.initial_js_max_bytes(),
    ) {
        let mut lines = vec!["performance budget exceeded:".to_string()];
        for v in &violations {
            lines.push(format!(
                "  {} is {} bytes (budget {} bytes)",
                v.file, v.size, v.budget
            ));
        }
        return Err(NuError::PackageError {
            msg: lines.join("\n"),
            span: Span::default(),
        });
    }

    println!("Web build succeeded: {}", output_dir.display());
    Ok(())
}

fn route_path_to_output_file(path: &str) -> PathBuf {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        PathBuf::from("index.html")
    } else {
        PathBuf::from(trimmed).join("index.html")
    }
}

fn copy_dir_contents(src: &Path, dst: &Path) -> NuResult<()> {
    for entry in std::fs::read_dir(src)
        .map_err(|e| NuError::PackageError {
            msg: format!("cannot read {}: {}", src.display(), e),
            span: Span::default(),
        })?
        .flatten()
    {
        let path = entry.path();
        let dest = dst.join(path.file_name().unwrap_or(path.as_os_str()));
        if path.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| NuError::PackageError {
                msg: format!("cannot create {}: {}", dest.display(), e),
                span: Span::default(),
            })?;
            copy_dir_contents(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).map_err(|e| NuError::PackageError {
                msg: format!(
                    "cannot copy {} to {}: {}",
                    path.display(),
                    dest.display(),
                    e
                ),
                span: Span::default(),
            })?;
        }
    }
    Ok(())
}

/// `nula dev`: compile the entry point, collect `Web.route` registrations,
/// then start a dev HTTP server on the configured port. Routes are dispatched
/// to their handler functions; unmatched paths fall back to static files.
fn cmd_dev(port_override: Option<u16>) -> NuResult<()> {
    let root = package_root()?;
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", MANIFEST_FILE, e),
        span: Span::default(),
    })?;
    let port = port_override.unwrap_or(manifest.web.port);

    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();

    let dev_dir = root.join(".nula");
    std::fs::create_dir_all(&dev_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", dev_dir.display(), e),
        span: Span::default(),
    })?;
    let nbc_path = dev_dir.join("dev.nbc");
    let nbc_path_str = nbc_path.to_string_lossy().into_owned();

    let static_dir = root.join(&manifest.web.static_dir);

    // Emit the compile-time signal graph and client micro-runtime for the dev
    // server too. Dynamic routes won't read them, but client-side progressive
    // enhancement and the static fallback both need them in the output dir.
    let output_dir = root.join(&manifest.web.output_dir);
    std::fs::create_dir_all(&output_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", output_dir.display(), e),
        span: Span::default(),
    })?;

    let signals_path = output_dir.join("app.signals.json");
    let client_js_path = output_dir.join("app.client.js");

    eprintln!("Compiling {} for dev...", entry.display());
    nulang_exe(&[
        "--emit-nbc",
        "--out",
        &nbc_path_str,
        "--rewrite-signals",
        &client_js_path.to_string_lossy(),
        &entry_str,
    ])?;

    let bytes = std::fs::read(&nbc_path).map_err(|e| NuError::PackageError {
        msg: format!("cannot read {}: {}", nbc_path.display(), e),
        span: Span::default(),
    })?;
    let artifact = CodeModule::from_nbc(&bytes).map_err(|e| NuError::PackageError {
        msg: format!("cannot decode {}: {}", nbc_path.display(), e),
        span: Span::default(),
    })?;

    let mut vm = VM::new();
    vm.load_module(artifact.module);
    vm.run().map_err(|e| NuError::PackageError {
        msg: format!("runtime error in {}: {}", entry.display(), e),
        span: Span::default(),
    })?;
    let routes = vm.take_web_routes();

    nulang_exe(&[
        "--emit-signals",
        &signals_path.to_string_lossy(),
        &entry_str,
    ])?;

    if routes.is_empty() {
        cmd_build_web()?;
        let output_dir = root.join(&manifest.web.output_dir);
        let addr = format!("127.0.0.1:{}", port);
        let listener = std::net::TcpListener::bind(&addr).map_err(|e| NuError::PackageError {
            msg: format!("cannot bind dev server to {}: {}", addr, e),
            span: Span::default(),
        })?;
        println!("Serving static files at http://{}/", addr);
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let root = output_dir.clone();
                    std::thread::spawn(move || {
                        serve_static_file(stream, &root);
                    });
                }
                Err(e) => eprintln!("connection error: {}", e),
            }
        }
        Ok(())
    } else {
        let server = WebDevServer::bind(port, Some(static_dir), Some(output_dir.clone()), routes)
            .map_err(|e| NuError::PackageError {
            msg: format!("cannot bind dev server: {}", e),
            span: Span::default(),
        })?;
        let actual_port = server.port;
        println!("Dev server listening on http://127.0.0.1:{}/", actual_port);
        loop {
            std::thread::park();
        }
    }
}

fn serve_static_file(mut stream: std::net::TcpStream, root: &Path) {
    use std::io::{BufRead, BufReader, Write};
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    let path = parts.get(1).unwrap_or(&"/").trim_start_matches('/');
    let file_path = if path.is_empty() {
        root.join("index.html")
    } else {
        root.join(path)
    };
    let (status, content_type, body) = if file_path.is_file() {
        match std::fs::read(&file_path) {
            Ok(data) => ("200 OK", guess_content_type(&file_path), data),
            Err(_) => (
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"Not found".to_vec(),
            ),
        }
    } else {
        (
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"Not found".to_vec(),
        )
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// `nula test [--filter <substr>] [--verbose|-v]`: discover and run `.nula`
/// test files under the package's `tests/` directory, reporting pass/fail.
///
/// Each test file is executed via the `nulang` exe in the current package
/// (same process as `nula run`). A test PASSes if it runs to completion
/// without error; any compile or runtime error (including assertion
/// failures from the `Test` effect) is a FAIL.
///
/// With `--verbose` (or `-v`): prints each test file name before execution,
/// shows ✓ PASS / ✗ FAIL per file, and displays error messages for failures.
/// Default (non-verbose) output is clean and greppable.
fn cmd_test(filter: Option<&str>, verbose: bool, json: bool) -> NuResult<()> {
    eprintln!("Preparing package...");
    let _entry = prepare_package()?;
    let tests_dir = package_root()?.join("tests");
    let mut test_files: Vec<PathBuf> = match std::fs::read_dir(&tests_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "nula"))
            .filter(|p| {
                filter.map_or(true, |f| {
                    p.file_stem()
                        .and_then(|s| s.to_str())
                        .map_or(false, |s| s.contains(f))
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    test_files.sort();
    if test_files.is_empty() {
        if json {
            let mut report = crate::json_diagnostics::JsonReport::new("test", None, Vec::new());
            report.tests = Some(Vec::new());
            print!("{}", report.to_json_string());
        } else {
            println!("No tests found in {}", tests_dir.display());
        }
        return Ok(());
    }

    // Phase 1: discover per-function tests (fn test_*)
    struct TestCase {
        display: String,
        file_to_run: PathBuf,
        is_temp: bool,
    }

    let mut tests: Vec<TestCase> = Vec::new();
    let temp_dir = std::env::temp_dir().join("nula_test");
    let _ = std::fs::create_dir_all(&temp_dir);

    for file in &test_files {
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let test_fns = discover_test_functions(&content);
        let relative = file
            .strip_prefix(&tests_dir.parent().unwrap_or(&tests_dir))
            .unwrap_or(file);

        if test_fns.is_empty() {
            // No test_* functions: run whole file as one test
            tests.push(TestCase {
                display: relative.display().to_string(),
                file_to_run: file.clone(),
                is_temp: false,
            });
        } else {
            if verbose {
                println!("--- {} ---", relative.display());
                println!("  discovered: {}", test_fns.join(", "));
            }
            // Strip fn main() for per-function wrappers
            let stripped = strip_main_function(&content);
            for fn_name in &test_fns {
                let wrapper = format!(
                    "{}{}\nfn main() {{ {}() }}\n",
                    stripped,
                    if stripped.ends_with('\n') { "" } else { "\n" },
                    fn_name
                );
                let temp_path = temp_dir.join(format!("test_{}.nula", fn_name));
                if std::fs::write(&temp_path, &wrapper).is_err() {
                    continue;
                }
                tests.push(TestCase {
                    display: fn_name.clone(),
                    file_to_run: temp_path,
                    is_temp: true,
                });
            }
        }
    }

    eprintln!("running {} tests", tests.len());
    let mut passed = 0;
    let mut failed = 0;
    let mut json_results: Vec<crate::json_diagnostics::JsonTestResult> = Vec::new();

    for test in &tests {
        let file_str = test.file_to_run.to_string_lossy().into_owned();
        let started = std::time::Instant::now();
        // In --json mode the child's stdout is captured (discarded) so the
        // JSON report is the only thing on our stdout.
        let outcome = if json {
            run_test_file_captured(&file_str)
        } else {
            run_test_file(&file_str)
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(()) => {
                passed += 1;
                if json {
                    json_results.push(crate::json_diagnostics::JsonTestResult {
                        name: test.display.clone(),
                        status: "ok".to_string(),
                        duration_ms,
                        diagnostics: Vec::new(),
                    });
                } else {
                    println!("test {} ... ok", test.display);
                }
            }
            Err(stderr_output) => {
                failed += 1;
                if json {
                    json_results.push(crate::json_diagnostics::JsonTestResult {
                        name: test.display.clone(),
                        status: "failed".to_string(),
                        duration_ms,
                        diagnostics: vec![crate::json_diagnostics::diagnostic_from_message(
                            stderr_output.trim().to_string(),
                        )],
                    });
                } else if verbose {
                    println!("test {} ... FAILED", test.display);
                    for line in stderr_output.lines() {
                        println!("   {}", line);
                    }
                } else {
                    println!("test {} ... FAILED", test.display);
                    eprintln!("{}", stderr_output.trim());
                }
            }
        }
    }

    // Clean up temp files
    for test in &tests {
        if test.is_temp {
            let _ = std::fs::remove_file(&test.file_to_run);
        }
    }
    let _ = std::fs::remove_dir(&temp_dir);

    if json {
        let diagnostics = json_results
            .iter()
            .flat_map(|t| t.diagnostics.clone())
            .collect();
        let mut report = crate::json_diagnostics::JsonReport::new("test", None, diagnostics);
        report.tests = Some(json_results);
        print!("{}", report.to_json_string());
        if failed > 0 {
            return Err(NuError::PackageError {
                msg: format!("{} test(s) failed", failed),
                span: Span::default(),
            });
        }
        return Ok(());
    }

    println!("\ntest result: {} passed; {} failed", passed, failed);
    if failed > 0 {
        return Err(NuError::PackageError {
            msg: format!("{} test(s) failed", failed),
            span: Span::default(),
        });
    }
    Ok(())
}

/// `nula test --watch` (or `nula test -w`): run tests and re-run when source
/// files change under `src/` or `tests/`. Uses simple mtime polling.
fn cmd_test_watch(filter: Option<&str>, verbose: bool) -> NuResult<()> {
    let root = package_root()?;

    // Initial run
    let _ = cmd_test(filter, verbose, false);

    // Collect initial mtimes for all .nula files under src/ and tests/
    let src_dir = root.join("src");
    let tests_dir = root.join("tests");
    let mut last_src = collect_mtimes(&src_dir);
    let mut last_tests = collect_mtimes(&tests_dir);

    println!("watching for changes... (Ctrl-C to stop)");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let current_src = collect_mtimes(&src_dir);
        let current_tests = collect_mtimes(&tests_dir);
        if current_src != last_src || current_tests != last_tests {
            last_src = current_src;
            last_tests = current_tests;
            // Clear screen
            print!("\x1B[2J\x1B[H");
            eprintln!("re-running tests...");
            let _ = cmd_test(filter, verbose, false);
        }
    }
}

/// Find all `fn test_*` function names in a source string.
fn discover_test_functions(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = source;
    while let Some(pos) = rest.find("fn test_") {
        let start = pos + 3; // skip "fn "
        let after_fn = &rest[start..];
        // Find end of identifier: whitespace or '('
        let end = after_fn
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(after_fn.len());
        let name = after_fn[..end].trim().to_string();
        if !name.is_empty() {
            names.push(name);
        }
        rest = &after_fn[end..];
    }
    names
}

/// Strip the `fn main() { ... }` block from source, keeping everything else.
fn strip_main_function(source: &str) -> String {
    if let Some(pos) = source.find("fn main") {
        // Find the opening brace after "fn main"
        if let Some(brace_start) = source[pos..].find('{') {
            let abs_brace = pos + brace_start;
            // Count braces to find matching close
            let mut depth = 0;
            let mut end = abs_brace;
            for (i, ch) in source[abs_brace..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = abs_brace + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let before = &source[..pos];
            let after = &source[end..];
            return format!("{}{}", before, after);
        }
    }
    source.to_string()
}
/// Run a test file via `nulang`, capturing stderr so error messages appear
/// after the test name (avoiding interleaved output).
/// Returns `Ok(())` on success, `Err(stderr_string)` on failure.
/// Like [`run_test_file`], but captures (discards) the child's stdout so the
/// parent's stdout stays clean for `--json` output.
fn run_test_file_captured(file_path: &str) -> Result<(), String> {
    run_test_file_impl(file_path, false)
}

fn run_test_file(file_path: &str) -> Result<(), String> {
    run_test_file_impl(file_path, true)
}

fn run_test_file_impl(file_path: &str, inherit_stdout: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate nulang: {}", e))?;
    let mut cmd = Command::new(&exe);
    cmd.arg(file_path);
    cmd.args(capability_args());
    if inherit_stdout {
        cmd.stdout(std::process::Stdio::inherit());
    } else {
        cmd.stdout(std::process::Stdio::piped());
    }
    cmd.stderr(std::process::Stdio::piped());
    if std::env::var_os("NULANG_STDLIB").is_none() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("stdlib");
            if candidate.is_dir() {
                cmd.env("NULANG_STDLIB", &candidate);
            } else {
                // Development fallback: when running the freshly-built `nula`
                // binary from the source tree, stdlib lives at src/stdlib/.
                let dev = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("src")
                    .join("stdlib");
                if dev.is_dir() {
                    cmd.env("NULANG_STDLIB", &dev);
                }
            }
        }
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run nulang: {}", e))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Err(stderr)
    }
}

/// `nula list`: print all locked dependencies with versions and sources.
fn cmd_list() -> NuResult<()> {
    let root = package_root()?;
    let lock_path = root.join(LOCKFILE_FILE);
    let lockfile = Lockfile::load(&root).map_err(|e| NuError::PackageError {
        msg: format!(
            "failed to read {}: {}\n  hint: run 'nulang nula build' first to generate it",
            lock_path.display(),
            e
        ),
        span: Span::default(),
    })?;
    if lockfile.package.is_empty() {
        println!("No dependencies locked.");
        return Ok(());
    }
    println!("Locked dependencies (from {}):", lock_path.display());
    for pkg in &lockfile.package {
        println!("  {} v{} — {}", pkg.name, pkg.version, pkg.source);
    }
    Ok(())
}

/// `nula clean`: remove build artifacts (.nbc files).
/// `nula clean`: remove build artifacts (.nula/dist/ directory).
fn cmd_clean() -> NuResult<()> {
    let root = package_root()?;
    let dist_dir = root.join(".nula").join("dist");
    if dist_dir.exists() {
        eprintln!("Cleaning build artifacts...");
        std::fs::remove_dir_all(&dist_dir).map_err(|e| NuError::PackageError {
            msg: format!("cannot remove {}: {}", dist_dir.display(), e),
            span: Span::default(),
        })?;
        println!("Removed build artifacts.");
    } else {
        println!("No build artifacts found.");
    }
    Ok(())
}

/// `nula doc [--open]`: generate Markdown API docs for the package.
///
/// Scans all `.nula` files under `src/`, extracts doc comments (`///` and
/// `//!`) and declarations (`fn`, `actor`, `type`, `workflow`), and writes
/// a combined `docs/api.md`. With `--open`, spawns `xdg-open` on the
/// output file (best-effort).
fn cmd_doc(open: bool) -> NuResult<()> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(NuError::PackageError {
            msg: format!(
                "no {} found in {} — run 'nulang nula init' first",
                MANIFEST_FILE,
                root.display()
            ),
            span: Span::default(),
        });
    }
    let out_path = crate::docgen::write_package_docs(&root)?;
    println!("Wrote {}", out_path.display());
    if open {
        let _ = std::process::Command::new("xdg-open")
            .arg(out_path.to_string_lossy().as_ref())
            .spawn();
    }
    Ok(())
}

/// Recursively remove .nbc files under `dir`.

/// `nula publish [--registry <url>] [--token <token>]` — package and upload
/// the current package to a registry.
fn cmd_publish(registry_url: Option<String>, token: Option<String>) -> NuResult<()> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(NuError::PackageError {
            msg: format!(
                "no {} found in {} — run 'nulang nula init' first",
                MANIFEST_FILE,
                root.display()
            ),
            span: Span::default(),
        });
    }
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;

    let registry_url = registry_url
        .or(manifest.package.registry.clone())
        .or_else(|| std::env::var("NULA_REGISTRY").ok())
        .ok_or_else(|| NuError::PackageError {
            msg: "No registry URL — set `registry` in [package] of Nulang.toml, pass --registry, or set NULA_REGISTRY env var".to_string(),
            span: Span::default(),
        })?;

    let token = token
        .or_else(|| std::env::var("NULA_TOKEN").ok())
        .ok_or_else(|| NuError::PackageError {
            msg: "No auth token — pass --token or set NULA_TOKEN env var".to_string(),
            span: Span::default(),
        })?;

    // Build tarball of the package (Nulang.toml + src/ tree).
    let mut tarball = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);

        // Add Nulang.toml
        ar.append_path_with_name(&manifest_path, "Nulang.toml")
            .map_err(|e| NuError::PackageError {
                msg: format!("cannot package {}: {}", MANIFEST_FILE, e),
                span: Span::default(),
            })?;

        // Add src/ tree
        let src_dir = root.join("src");
        if src_dir.is_dir() {
            add_dir_to_tar(&mut ar, &src_dir, "src").map_err(|e| NuError::PackageError {
                msg: format!("cannot package src/: {}", e),
                span: Span::default(),
            })?;
        }

        // Add tests/ if present
        let tests_dir = root.join("tests");
        if tests_dir.is_dir() {
            add_dir_to_tar(&mut ar, &tests_dir, "tests").map_err(|e| NuError::PackageError {
                msg: format!("cannot package tests/: {}", e),
                span: Span::default(),
            })?;
        }

        let gz = ar.into_inner().map_err(|e| NuError::PackageError {
            msg: format!("cannot finish tarball: {}", e),
            span: Span::default(),
        })?;
        gz.finish().map_err(|e| NuError::PackageError {
            msg: format!("cannot compress tarball: {}", e),
            span: Span::default(),
        })?;
    }

    let name = &manifest.package.name;
    let version = &manifest.package.version;
    eprintln!("Publishing {}-{} to {} ...", name, version, registry_url);

    let client = RegistryClient::new(registry_url, Some(token));
    client
        .publish(name, version, &tarball)
        .map_err(|e| NuError::PackageError {
            msg: format!("publish failed: {}", e),
            span: Span::default(),
        })?;

    println!("Published {}-{} successfully.", name, version);
    Ok(())
}
/// Response from POST /api/v1/deploy on Nulang Cloud.
#[cfg(feature = "ureq")]
#[derive(serde::Deserialize)]
struct DeployResponse {
    #[allow(dead_code)]
    deployment_id: String,
    url: String,
    status: String,
}

/// `nula deploy [--wasm] [--url <url>] [--token <token>] [--adapter <kind>] [--dry-run]`
/// — build and deploy the current package to Nulang Cloud or another adapter.
#[cfg(feature = "ureq")]
fn cmd_deploy(
    wasm: bool,
    cloud_url: Option<String>,
    token: Option<String>,
    adapter: crate::web::adapters::AdapterKind,
    dry_run: bool,
) -> NuResult<()> {
    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(NuError::PackageError {
            msg: format!("No {} found. Run `nulang nula init` first.", MANIFEST_FILE),
            span: Span::default(),
        });
    }
    let manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;
    let name = manifest.package.name.clone();

    // Build the web output so dist/ contains the IR and static assets.
    cmd_build_web()?;

    let dist_dir = root.join(&manifest.web.output_dir);

    // Generate adapter-specific files in dist/.
    let ir_path = dist_dir.join("nulang-app.ir.json");
    let ir: crate::web::ir::DeploymentIr = if ir_path.exists() {
        let s = std::fs::read_to_string(&ir_path).map_err(|e| NuError::PackageError {
            msg: format!("cannot read {}: {}", ir_path.display(), e),
            span: Span::default(),
        })?;
        serde_json::from_str(&s).unwrap_or_default()
    } else {
        crate::web::ir::DeploymentIr {
            version: 1,
            routes: Vec::new(),
            signals: serde_json::Value::Object(Default::default()),
            capabilities: Vec::new(),
            budgets: Default::default(),
            middleware: Vec::new(),
            cloud_config: Vec::new(),
        }
    };
    match adapter {
        crate::web::adapters::AdapterKind::NulangCloud => {}
        crate::web::adapters::AdapterKind::StaticHost => {
            crate::web::adapters::static_host::generate_files(&dist_dir, &ir).map_err(|e| {
                NuError::PackageError {
                    msg: format!("cannot generate static-host files: {}", e),
                    span: Span::default(),
                }
            })?;
        }
        crate::web::adapters::AdapterKind::Docker => {
            crate::web::adapters::docker::generate_files(&dist_dir).map_err(|e| {
                NuError::PackageError {
                    msg: format!("cannot generate docker files: {}", e),
                    span: Span::default(),
                }
            })?;
        }
    }

    // Build artifacts into .nula/dist/.
    let nula_dist = root.join(".nula").join("dist");
    std::fs::create_dir_all(&nula_dist).map_err(|e| NuError::PackageError {
        msg: format!("cannot create {}: {}", nula_dist.display(), e),
        span: Span::default(),
    })?;

    // Always build .nbc (native bytecode tier).
    let entry = prepare_package()?;
    let entry_str = entry.to_string_lossy().into_owned();
    let nbc_path = nula_dist.join(format!("{}.nbc", name));
    let nbc_path_str = nbc_path.to_string_lossy().into_owned();
    eprintln!("Compiling {} to .nbc...", name);
    nulang_exe(&["--emit-nbc", "--out", &nbc_path_str, &entry_str])?;

    // Optionally build .wasm + .cwasm (WASM tier).
    if wasm {
        let wasm_path = nula_dist.join(format!("{}.wasm", name));
        let wasm_path_str = wasm_path.to_string_lossy().into_owned();
        eprintln!("Compiling {} to .wasm + .cwasm...", name);
        nulang_exe(&["--backend", "wasm-aot", "--out", &wasm_path_str, &entry_str])?;
    }

    // Bundle into .tar.gz: .nula/dist/ contents + dist/** + Nulang.toml + Nulang.lock.
    let mut tarball = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);

        ar.append_path_with_name(&manifest_path, "Nulang.toml")
            .map_err(|e| NuError::PackageError {
                msg: format!("cannot package {}: {}", MANIFEST_FILE, e),
                span: Span::default(),
            })?;

        let lock_path = root.join(LOCKFILE_FILE);
        if lock_path.exists() {
            ar.append_path_with_name(&lock_path, LOCKFILE_FILE)
                .map_err(|e| NuError::PackageError {
                    msg: format!("cannot package {}: {}", LOCKFILE_FILE, e),
                    span: Span::default(),
                })?;
        }

        if dist_dir.is_dir() {
            ar.append_dir_all("dist", &dist_dir)
                .map_err(|e| NuError::PackageError {
                    msg: format!("cannot package {}: {}", dist_dir.display(), e),
                    span: Span::default(),
                })?;
        }

        if nula_dist.is_dir() {
            ar.append_dir_all(".nula/dist", &nula_dist)
                .map_err(|e| NuError::PackageError {
                    msg: format!("cannot package {}: {}", nula_dist.display(), e),
                    span: Span::default(),
                })?;
        }

        let gz = ar.into_inner().map_err(|e| NuError::PackageError {
            msg: format!("cannot finish tarball: {}", e),
            span: Span::default(),
        })?;
        gz.finish().map_err(|e| NuError::PackageError {
            msg: format!("cannot compress tarball: {}", e),
            span: Span::default(),
        })?;
    }

    if dry_run {
        let contents = list_tarball_contents(&tarball);
        println!("Dry-run deploy ({}) — tarball entries:", adapter.as_str());
        for entry in &contents {
            println!("  {}", entry);
        }
        if !ir.cloud_config.is_empty() {
            println!("Required cloud config:");
            for entry in &ir.cloud_config {
                println!("  {} (required by {})", entry.key, entry.required_by);
            }
        }
        println!(
            "Dry-run: would deploy {} bytes (adapter: {})",
            tarball.len(),
            adapter.as_str()
        );
        return Ok(());
    }

    // Token required for actual deploy.
    let token = token
        .or_else(|| std::env::var("NULANG_CLOUD_TOKEN").ok())
        .ok_or_else(|| NuError::PackageError {
            msg: "Set NULANG_CLOUD_TOKEN or pass --token".to_string(),
            span: Span::default(),
        })?;

    let cloud_url = cloud_url
        .or_else(|| std::env::var("NULANG_CLOUD_URL").ok())
        .unwrap_or_else(|| "https://deploy.nulang.cloud".to_string());

    eprintln!("Deploying {} to {} ...", name, cloud_url);

    let url = format!("{}/api/v1/deploy", cloud_url.trim_end_matches('/'));
    let response: ureq::Response = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", token))
        .set("Content-Type", "application/gzip")
        .send_bytes(&tarball)
        .map_err(|e| match e {
            ureq::Error::Transport(inner) => NuError::PackageError {
                msg: format!("Failed to connect to {}: {}", cloud_url, inner),
                span: Span::default(),
            },
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                NuError::PackageError {
                    msg: if code == 401 {
                        "Authentication failed. Check your NULANG_CLOUD_TOKEN.".to_string()
                    } else {
                        format!("Deploy failed: {} — {}", code, body)
                    },
                    span: Span::default(),
                }
            }
        })?;

    let body = response.into_string().map_err(|e| NuError::PackageError {
        msg: format!("failed to read response: {}", e),
        span: Span::default(),
    })?;
    let deploy: DeployResponse =
        serde_json::from_str(&body).map_err(|e| NuError::PackageError {
            msg: format!("invalid response from cloud: {}", e),
            span: Span::default(),
        })?;

    println!("Deployed! -> {} ({})", deploy.url, deploy.status);
    Ok(())
}

/// `nula deploy` — disabled without the `ureq` feature.
#[cfg(not(feature = "ureq"))]
fn cmd_deploy(
    _wasm: bool,
    _cloud_url: Option<String>,
    _token: Option<String>,
    _adapter: crate::web::adapters::AdapterKind,
    _dry_run: bool,
) -> NuResult<()> {
    Err(NuError::PackageError {
        msg: "cloud deploy requires the 'ureq' feature (build with --features ureq)".to_string(),
        span: Span::default(),
    })
}

/// List the paths stored in an in-memory gzip-compressed tarball.
fn list_tarball_contents(tarball: &[u8]) -> Vec<String> {
    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(tarball));
    let mut ar = tar::Archive::new(gz);
    let entries = match ar.entries() {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    entries
        .filter_map(|e| {
            let e = e.ok()?;
            Some(e.path().ok()?.display().to_string())
        })
        .collect()
}

/// Recursively add a directory tree to a tar builder.
fn add_dir_to_tar<W: std::io::Write>(
    ar: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &str,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = format!("{}/{}", prefix, entry.file_name().to_string_lossy());
        if path.is_dir() {
            add_dir_to_tar(ar, &path, &rel)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("nula") {
            ar.append_path_with_name(&path, &rel)?;
        }
    }
    Ok(())
}

/// `nula add <name> [--path <p>] [--git <url>] [--version <v>]` — add or
/// update a dependency in `Nulang.toml`, then re-resolve and update
/// `Nulang.lock`.
fn cmd_add(
    name: Option<&String>,
    path: Option<&str>,
    git: Option<&str>,
    version: Option<&str>,
) -> NuResult<()> {
    let name = name.ok_or_else(|| NuError::PackageError {
        msg: "nula add requires a dependency name".to_string(),
        span: Span::default(),
    })?;
    validate_package_name(name)?;

    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(NuError::PackageError {
            msg: format!(
                "no {} found in {} — run 'nulang nula init' first",
                MANIFEST_FILE,
                root.display()
            ),
            span: Span::default(),
        });
    }
    let mut manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;

    let dep = if path.is_some() || git.is_some() {
        Dependency::Detailed(DependencyDetail {
            path: path.map(|s| s.to_string()),
            git: git.map(|s| s.to_string()),
            version: version.map(|s| s.to_string()),
            ..Default::default()
        })
    } else {
        // Bare version dependency (or no flags -> version "*")
        Dependency::Version(version.unwrap_or("*").to_string())
    };

    let updated = manifest
        .dependencies
        .insert(name.to_string(), dep)
        .is_some();
    manifest.save(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to write {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;

    if updated {
        println!("Updated dependency '{}' in {}.", name, MANIFEST_FILE);
    } else {
        println!("Added dependency '{}' to {}.", name, MANIFEST_FILE);
    }

    // Re-resolve and update the lockfile.
    eprintln!("  Resolving dependencies...");
    let resolution = resolve(&root, &manifest).map_err(|e| NuError::PackageError {
        msg: format!("failed to resolve dependencies: {}", e),
        span: Span::default(),
    })?;
    resolution
        .to_lockfile()
        .save(&root)
        .map_err(|e| NuError::PackageError {
            msg: format!(
                "failed to write {}: {}",
                root.join(LOCKFILE_FILE).display(),
                e
            ),
            span: Span::default(),
        })?;
    println!("  Lockfile updated.");
    Ok(())
}

/// `nula remove <name>` — remove a dependency from `Nulang.toml` and update
/// the lockfile.
fn cmd_remove(name: Option<&str>) -> NuResult<()> {
    let name = name.ok_or_else(|| NuError::PackageError {
        msg: "nula remove requires a dependency name".to_string(),
        span: Span::default(),
    })?;

    let root = package_root()?;
    let manifest_path = root.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(NuError::PackageError {
            msg: format!(
                "no {} found in {} — run 'nulang nula init' first",
                MANIFEST_FILE,
                root.display()
            ),
            span: Span::default(),
        });
    }
    let mut manifest = Manifest::load(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to load {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;

    if manifest.dependencies.remove(name).is_none() {
        return Err(NuError::PackageError {
            msg: format!("dependency '{}' not found in {}", name, MANIFEST_FILE),
            span: Span::default(),
        });
    }

    manifest.save(&root).map_err(|e| NuError::PackageError {
        msg: format!("failed to write {}: {}", manifest_path.display(), e),
        span: Span::default(),
    })?;
    println!("Removed dependency '{}' from {}.", name, MANIFEST_FILE);

    // Re-resolve and update the lockfile.
    eprintln!("  Resolving dependencies...");
    let resolution = resolve(&root, &manifest).map_err(|e| NuError::PackageError {
        msg: format!("failed to resolve dependencies: {}", e),
        span: Span::default(),
    })?;
    resolution
        .to_lockfile()
        .save(&root)
        .map_err(|e| NuError::PackageError {
            msg: format!(
                "failed to write {}: {}",
                root.join(LOCKFILE_FILE).display(),
                e
            ),
            span: Span::default(),
        })?;
    println!("  Lockfile updated.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_language_pin() {
        let dir = std::env::temp_dir().join(format!("nulang_lang_pin_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        scaffold_package(&dir, "test-pkg", "default").expect("scaffold should succeed");

        // 1. Absent language => ok
        assert!(prepare_package().is_ok());

        // 2. Matching language => ok (our format is 1.0.0-frozen, so 1.0.0 should match)
        let mut manifest = Manifest::load(&dir).expect("manifest should load");
        manifest.package.language = Some("1.0.0".to_string());
        manifest.save(&dir).unwrap();
        assert!(prepare_package().is_ok());

        // 3. Mismatching language => fail
        manifest.package.language = Some("2.0.0".to_string());
        manifest.save(&dir).unwrap();
        let err = prepare_package().expect_err("should fail with wrong language version");
        let msg = err.to_string();
        assert!(msg.contains("requires language 2.0.0"));
        assert!(msg.contains("this toolchain provides"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::package::manifest::DEFAULT_ENTRY;

    #[test]
    fn test_scaffold_package_creates_valid_manifest() {
        let dir = std::env::temp_dir().join(format!("nulang_nula_new_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        scaffold_package(&dir, "my-app", "default").expect("scaffold should succeed");
        let manifest = Manifest::load(&dir).expect("scaffolded manifest should parse");
        assert_eq!(manifest.package.name, "my-app");
        assert_eq!(manifest.package.version, "0.1.0");
        assert_eq!(manifest.package.entry, DEFAULT_ENTRY);
        assert!(dir.join(DEFAULT_ENTRY).exists());

        let resolution = resolve(&dir, &manifest).expect("scaffold should resolve");
        assert_eq!(resolution.root().name, "my-app");
        assert!(resolution.to_lockfile().package.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_new_rejects_invalid_name() {
        // Path with invalid package name (contains '.')
        let err = cmd_new(Some("./my.app"), None).expect_err("dots in name are rejected");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));
        let err = cmd_new(None, None).expect_err("missing name is rejected");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));
    }

    #[test]
    fn test_cmd_new_accepts_path() {
        let dir = std::env::temp_dir().join(format!("nulang_new_path_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path_str = dir.to_str().expect("temp dir should be valid UTF-8");
        let result = cmd_new(Some(path_str), None);
        assert!(
            result.is_ok(),
            "path with valid basename should succeed: {:?}",
            result.err()
        );
        assert!(dir.join("Nulang.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_all_templates_scaffold_a_main_nula() {
        // Every supported template must scaffold a non-empty `src/main.nula`
        // that references a `main` entry point. Guards against a template
        // arm being added to `template_files` but forgotten in the valid
        // list (or vice versa).
        for name in [
            "default",
            "cli",
            "lib",
            "full",
            "distributed",
            "ai-agent",
            "web",
        ] {
            let dir = std::env::temp_dir().join(format!(
                "nulang_tmpl_test_{}_{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            scaffold_package(&dir, "tmpl-app", name)
                .expect(&format!("template '{}' should scaffold", name));
            let main = dir.join("src/main.nula");
            assert!(
                main.exists(),
                "template '{}' must provide src/main.nula",
                name
            );
            let src = std::fs::read_to_string(&main).expect("read main.nula");
            assert!(
                src.contains("main") || src.contains("app "),
                "template '{}' main.nula must define an entry point",
                name
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_cmd_new_rejects_unknown_template() {
        let dir = std::env::temp_dir().join(format!("nulang_unk_tmpl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path_str = dir.to_str().unwrap();
        let err = cmd_new(Some(path_str), Some("does-not-exist"))
            .expect_err("unknown template must be rejected");
        assert!(err.to_string().contains("unknown template"));
        assert!(
            !dir.exists(),
            "nothing should be scaffolded for an unknown template"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_init_creates_in_current_dir() {
        let dir = std::env::temp_dir().join(format!("nulang_init_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        cmd_init().expect("init in empty dir should succeed");
        assert!(dir.join("Nulang.toml").exists());
        assert!(dir.join("src/main.nula").exists());
        assert!(dir.join(".gitignore").exists());

        // Second init should fail
        let err = cmd_init().expect_err("second init should fail");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_print_usage_does_not_panic() {
        print_usage();
    }

    #[test]
    fn test_nulang_exe_rejects_invalid_args() {
        let result = nulang_exe(&["--nonexistent-flag"]);
        assert!(result.is_err(), "unknown flags should fail");
    }

    #[test]
    fn test_cmd_test_fails_in_non_package_dir() {
        // Use a temp dir with no Nulang.toml so prepare_package fails.
        let dir = std::env::temp_dir().join(format!("nulang_no_pkg_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        let result = cmd_test(None, false, false);
        assert!(result.is_err(), "test outside package should fail");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper: point `package_root()` at `dir` for this thread only, restoring
    /// the previous override on drop. Uses the thread-local override instead
    /// of `std::env::set_current_dir`, which is process-global and raced with
    /// unrelated parallel tests resolving `stdlib::*`/example files relative
    /// to `current_dir()` (random failures that passed in isolation).
    struct ChangeDir {
        original: Option<PathBuf>,
    }

    impl ChangeDir {
        fn new(dir: &Path) -> Self {
            let original =
                PACKAGE_ROOT_OVERRIDE.with(|c| c.borrow_mut().replace(dir.to_path_buf()));
            ChangeDir { original }
        }
    }

    impl Drop for ChangeDir {
        fn drop(&mut self) {
            PACKAGE_ROOT_OVERRIDE.with(|c| *c.borrow_mut() = self.original.take());
        }
    }

    #[test]
    fn test_cmd_add_and_remove_dependency() {
        let dir =
            std::env::temp_dir().join(format!("nulang_add_remove_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Create a stub dep package inside the test dir so the resolver can find it
        let dep_dir = dir.join("deps").join("mylib");
        scaffold_package(&dep_dir, "mylib", "default").expect("scaffold dep should succeed");

        let _guard = ChangeDir::new(&dir);
        scaffold_package(&dir, "test-pkg", "default").expect("scaffold should succeed");

        let manifest = Manifest::load(&dir).expect("manifest should load");
        assert!(manifest.dependencies.is_empty());

        // Add a path dep (relative: ./deps/mylib)
        let result = cmd_add(Some(&"mylib".to_string()), Some("./deps/mylib"), None, None);
        assert!(result.is_ok(), "add should succeed: {:?}", result.err());

        let manifest = Manifest::load(&dir).expect("manifest should load after add");
        assert!(manifest.dependencies.contains_key("mylib"));
        match &manifest.dependencies["mylib"] {
            Dependency::Detailed(d) => {
                assert_eq!(d.path.as_deref(), Some("./deps/mylib"));
            }
            _ => panic!("expected detailed dependency"),
        }

        // Remove the dep
        cmd_remove(Some("mylib")).expect("remove should succeed");
        let manifest = Manifest::load(&dir).expect("manifest should load after remove");
        assert!(!manifest.dependencies.contains_key("mylib"));

        // Remove a non-existent dep should fail
        let err = cmd_remove(Some("nonexistent")).expect_err("remove nonexistent should fail");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_manifest_add_dependency_direct() {
        // Test manifest-level mutation directly (avoiding resolver for git/version deps)
        let dir =
            std::env::temp_dir().join(format!("nulang_manifest_dep_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        scaffold_package(&dir, "test-pkg", "default").expect("scaffold should succeed");

        // Add a detailed git dependency
        let mut manifest = Manifest::load(&dir).expect("manifest should load");
        manifest.dependencies.insert(
            "json".to_string(),
            Dependency::Detailed(DependencyDetail {
                git: Some("https://github.com/example/json.nu.git".to_string()),
                version: Some("0.2.0".to_string()),
                ..Default::default()
            }),
        );
        manifest.save(&dir).expect("save should succeed");

        // Reload and verify
        let manifest2 = Manifest::load(&dir).expect("manifest should reload");
        match &manifest2.dependencies["json"] {
            Dependency::Detailed(d) => {
                assert_eq!(
                    d.git.as_deref(),
                    Some("https://github.com/example/json.nu.git")
                );
                assert_eq!(d.version.as_deref(), Some("0.2.0"));
            }
            _ => panic!("expected detailed dependency"),
        }

        // Add a version-only dependency via fresh mutable load
        let mut manifest3 = Manifest::load(&dir).expect("manifest should load");
        manifest3.dependencies.insert(
            "registry-dep".to_string(),
            Dependency::Version("1.0.0".to_string()),
        );
        manifest3.save(&dir).expect("save should succeed");

        let manifest4 = Manifest::load(&dir).expect("manifest should reload");
        assert_eq!(
            manifest4.dependencies["registry-dep"],
            Dependency::Version("1.0.0".to_string())
        );

        // Remove and verify
        let mut manifest5 = Manifest::load(&dir).expect("manifest should load");
        assert!(manifest5.dependencies.remove("registry-dep").is_some());
        manifest5.save(&dir).expect("save should succeed");

        let manifest6 = Manifest::load(&dir).expect("manifest should reload");
        assert!(!manifest6.dependencies.contains_key("registry-dep"));
        assert!(manifest6.dependencies.contains_key("json"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_add_rejects_invalid_name() {
        let dir =
            std::env::temp_dir().join(format!("nulang_add_invalid_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        scaffold_package(&dir, "test-pkg", "default").expect("scaffold should succeed");

        let err = cmd_add(Some(&"bad.name".to_string()), Some("./foo"), None, None)
            .expect_err("invalid name should fail");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_add_missing_name() {
        let dir =
            std::env::temp_dir().join(format!("nulang_add_no_name_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        scaffold_package(&dir, "test-pkg", "default").expect("scaffold should succeed");

        let err = cmd_add(None, Some("./foo"), None, None).expect_err("missing name should fail");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_doc_generates_api_md() {
        let dir = std::env::temp_dir().join(format!("nulang_doc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        scaffold_package(&dir, "doc-pkg", "default").expect("scaffold should succeed");

        // Add a documented function to src/main.nula
        let main_path = dir.join("src").join("main.nula");
        std::fs::write(
            &main_path,
            "/// Adds two numbers.\nfn add(a: Int, b: Int) -> Int { a + b }\n",
        )
        .expect("write main.nula");

        let result = cmd_doc(false);
        assert!(result.is_ok(), "cmd_doc should succeed: {:?}", result.err());

        let api_md = dir.join("docs").join("api.md");
        assert!(api_md.exists(), "docs/api.md should exist");

        let content = std::fs::read_to_string(&api_md).expect("read api.md");
        assert!(
            content.contains("`main`"),
            "should contain module heading: {}",
            content
        );
        assert!(
            content.contains("`add`"),
            "should contain function name: {}",
            content
        );
        assert!(
            content.contains("Adds two numbers"),
            "should contain doc comment: {}",
            content
        );
        assert!(
            content.contains("nulang nula doc"),
            "should contain attribution: {}",
            content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_doc_fails_outside_package() {
        let dir =
            std::env::temp_dir().join(format!("nulang_doc_no_pkg_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        let result = cmd_doc(false);
        assert!(result.is_err(), "doc outside package should fail");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_cmd_deploy_missing_token() {
        let dir =
            std::env::temp_dir().join(format!("nulang_deploy_token_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        scaffold_package(&dir, "deploy-test", "default").expect("scaffold should succeed");

        let result = cmd_deploy(
            false,
            None,
            None,
            crate::web::adapters::AdapterKind::NulangCloud,
            false,
        );
        assert!(result.is_err(), "deploy without token should fail");
        if let Err(NuError::PackageError { msg, .. }) = result {
            assert!(
                msg.contains("NULANG_CLOUD_TOKEN") || msg.contains("--token"),
                "error should mention token: {}",
                msg
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_deploy_no_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "nulang_deploy_nomanifest_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ChangeDir::new(&dir);

        let result = cmd_deploy(
            false,
            Some("http://127.0.0.1:9".to_string()),
            Some("t".to_string()),
            crate::web::adapters::AdapterKind::NulangCloud,
            false,
        );
        assert!(result.is_err(), "deploy without manifest should fail");
        if let Err(NuError::PackageError { msg, .. }) = result {
            assert!(
                msg.contains("Nulang.toml"),
                "error should mention Nulang.toml: {}",
                msg
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_web_signal_hydration() {
        let dir =
            std::env::temp_dir().join(format!("nulang_build_web_signal_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Scaffold a minimal web package and overwrite the entry with a signal example.
        scaffold_package(&dir, "signal-app", "web").expect("web scaffold should succeed");
        let main = dir.join("src/main.nula");
        std::fs::write(
            &main,
            r#"import stdlib::web::html
import stdlib::web::types

signal count: Html = text("0")

fn add() {}

fn home() -> Html {
    <div class="card">
        <button action={add}>Add</button>
        <span>{count}</span>
    </div>
}

app "counter" {
    route "GET" "/" -> home
}
"#,
        )
        .unwrap();

        let _guard = ChangeDir::new(&dir);
        cmd_build_web().expect("web build should succeed");

        let html =
            std::fs::read_to_string(dir.join("dist/index.html")).expect("index.html should exist");
        assert!(
            html.contains(r#"<script src="/app.client.js"></script>"#),
            "script tag missing"
        );
        assert!(html.contains("data-action="), "data-action missing");
        assert!(
            html.contains("data-action-placement="),
            "data-action-placement missing"
        );
        assert!(html.contains("data-signal="), "data-signal missing");

        let js = std::fs::read_to_string(dir.join("dist/app.client.js"))
            .expect("app.client.js should exist");
        assert!(js.contains("window.nulang"), "nulang global missing");
        assert!(js.contains("hydrate"), "hydrate missing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_deploy_dry_run_emits_ir_and_dist() {
        let dir = std::env::temp_dir().join(format!("nulang_deploy_dryrun_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        scaffold_package(&dir, "dryrun-app", "web").expect("web scaffold should succeed");

        let _guard = ChangeDir::new(&dir);
        let result = cmd_deploy(
            false,
            None,
            None,
            crate::web::adapters::AdapterKind::NulangCloud,
            true,
        );
        assert!(
            result.is_ok(),
            "dry-run deploy should succeed: {:?}",
            result.err()
        );

        assert!(
            dir.join("dist/nulang-app.ir.json").exists(),
            "IR file should be emitted"
        );
        let ir = std::fs::read_to_string(dir.join("dist/nulang-app.ir.json")).unwrap();
        assert!(ir.contains("\"version\": 1"), "IR should contain version");
        assert!(ir.contains("routes"), "IR should contain routes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_module_subcommand_dispatch_auth_enable() {
        let result = try_module_subcommand(&["auth".to_string(), "enable".to_string()]);
        assert!(
            result.is_some(),
            "auth enable should be recognized as a module subcommand"
        );
        assert!(result.unwrap().is_ok(), "auth enable should succeed");

        // Unknown subcommands should not be intercepted.
        assert!(try_module_subcommand(&["unknown".to_string()]).is_none());
    }
}

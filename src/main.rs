//! Nulang CLI entry point.
//!
//! Usage:
//!   nulang [OPTIONS] <FILE>
//!   nulang --repl
//!   nulang --eval <CODE>
//!   nulang --check <FILE>
//!   nulang --lsp
//!   nulang --dap [FILE]
//!   nulang agent <init|run|chat|goals|graph>
//!   nulang nula <new|build|build-wasm|test|run|add|remove|publish|deploy|watch|doc>
//!   nulang fmt [--check] [<file>]
//!
//! Options:
//!   -r, --repl               Start interactive REPL
//!   -e, --eval <CODE>        Evaluate a code string
//!   -c, --check <FILE>       Type-check a file (don't run)
//!   --doc                    Generate Markdown API docs (docs/api.md)
//!   --emit-stdlib-docs <dir> Generate per-effect stdlib docs into <dir>
//!   --lsp                    Start Language Server (stdio)
//!   --dap                    Start Debug Adapter (stdio); program from launch request or FILE
//!   --backend <b>            Backend: bytecode (default, full language) | native
//!                            (pure-functional subset only — effects/actors/FFI
//!                            error with a specific unsupported-construct message)
//!                            | wasm* (IO.print/read only; no user-defined effect
//!                            handlers, no actor mailbox — requires wasm-backend)
//!   --out <file>             Output file (WASM backends / --emit-nbc)
//!   --emit-nbc               Compile <FILE> to a .nbc artifact; don't run
//!   <FILE>.nbc               Run a pre-compiled .nbc artifact directly
//!   --verify <src>           Verify .nbc source hash against <src>
//!   nula <cmd>               Package manager (new, init, build, build-wasm, test, run, add, remove, publish, deploy, list, clean)
//!   --version, -V            Print version and exit
//!   -v, --verbose            Show bytecode and AST
//!   --bench [N]             Benchmark: run N times (default 10), print min/mean/median/max
//!   --color auto|always|never  Colorize error output (default: auto)
//!   -h, --help               Show this help message
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const VERSION: &str = "0.1.0";
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::repl::Repl;
use nulang::stdlib::StdLib;
use nulang::typechecker::TypeChecker;
use nulang::types::{NuError, NuResult, Span, Type};
use nulang::vm::VM;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Instant;
use tracing::instrument;
fn main() {
    // Initialize structured tracing (RUST_LOG env var controls verbosity).
    // Default level: warn (silent for normal runs). Users opt in with
    // RUST_LOG=nulang=debug or RUST_LOG=info.
    #[cfg(feature = "otel")]
    {
        // Forward spans to both the terminal and OTLP (when a tracer
        // provider has been configured). Fall back to terminal-only logging
        // if the subscriber cannot be installed.
        match nulang::observability::init_tracing("nulang-runtime") {
            Ok(()) => {}
            Err(e) => {
                eprintln!("OTLP tracing init failed ({e}); terminal logging only");
                use tracing_subscriber::{fmt, EnvFilter};
                let env_filter =
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
                // stderr, never stdout: `--lsp` must keep stdout pure
                // JSON-RPC framing, and CLI logs must not pollute piped
                // program output.
                fmt()
                    .with_env_filter(env_filter)
                    .with_target(false)
                    .with_writer(std::io::stderr)
                    .init();
            }
        }
    }
    #[cfg(not(feature = "otel"))]
    {
        use tracing_subscriber::{fmt, EnvFilter};
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        // stderr, never stdout: `--lsp` must keep stdout pure JSON-RPC
        // framing, and CLI logs must not pollute piped program output.
        fmt()
            .with_env_filter(env_filter)
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }

    let args: Vec<String> = std::env::args().collect();

    if args.len() <= 1 {
        // If stdin is piped, execute as a script; otherwise start REPL.
        if !std::io::stdin().is_terminal() {
            let mut source = String::new();
            std::io::stdin()
                .read_to_string(&mut source)
                .expect("Failed to read stdin");
            let opts = Options::default();
            let use_color = color_enabled(&opts);
            if let Err(e) = run_source(
                &source,
                None,
                opts.verbose,
                &opts.backend,
                opts.out_file.as_deref(),
                opts.metrics_port,
                &opts.target,
                &opts.with_capabilities,
                opts.store_path.as_deref(),
                opts.deny_warnings,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
            return;
        }
        let mut repl = Repl::new();
        repl.run();
        return;
    }

    // `nulang registry serve` — start a package registry server.
    if args.len() >= 3 && args[1] == "registry" && args[2] == "serve" {
        let mut bind = "127.0.0.1:8087".to_string();
        let mut data_dir = ".nula-registry".to_string();
        let mut auth_token: Option<String> = None;
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--bind" => {
                    i += 1;
                    if i < args.len() {
                        bind = args[i].clone();
                    }
                }
                "--dir" => {
                    i += 1;
                    if i < args.len() {
                        data_dir = args[i].clone();
                    }
                }
                "--token" => {
                    i += 1;
                    if i < args.len() {
                        auth_token = Some(args[i].clone());
                    }
                }
                other => {
                    eprintln!("Unknown registry serve option: {}", other);
                    std::process::exit(1);
                }
            }
            i += 1;
        }
        let server =
            nulang::registry::RegistryServer::new(std::path::PathBuf::from(&data_dir), auth_token);
        eprintln!("Registry listening on {} (data: {})", bind, data_dir);
        if let Err(e) = server.start(&bind) {
            eprintln!("Registry server error: {}", e);
            std::process::exit(1);
        }
        // Run until interrupted
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    // `nulang nula <cmd>` dispatches to the package manager.
    if args[1] == "fmt" {
        let mut check_mode = false;
        let mut file_arg: Option<&str> = None;
        let mut i = 2;
        while i < args.len() {
            if args[i] == "--check" {
                check_mode = true;
            } else if !args[i].starts_with('-') {
                file_arg = Some(&args[i]);
            } else {
                eprintln!("Unknown fmt option: {}", args[i]);
                std::process::exit(1);
            }
            i += 1;
        }

        if let Some(p) = file_arg {
            let s = std::fs::read_to_string(p).unwrap_or_else(|e| {
                eprintln!("Cannot read '{}': {}", p, e);
                std::process::exit(1);
            });
            match nulang::fmt::format_source(&s) {
                Ok(f) => {
                    if check_mode {
                        if f != s {
                            eprintln!("Would reformat {}", p);
                            std::process::exit(1);
                        }
                    } else {
                        if f != s {
                            std::fs::write(p, &f).unwrap_or_else(|e| {
                                eprintln!("Cannot write '{}': {}", p, e);
                                std::process::exit(1);
                            });
                            println!("Formatted {}", p);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{}: {}", p, e);
                    std::process::exit(1);
                }
            }
        } else {
            let dir = std::path::Path::new("src");
            if !dir.is_dir() {
                eprintln!("Not a package directory (no src/)");
                std::process::exit(1);
            }
            if let Err(e) = nulang::fmt::format_directory(dir, check_mode) {
                eprintln!("{}", e);
                std::process::exit(exit_code(&e));
            }
        }
        return;
    }
    // `nulang node --listen <ADDR> [--seed <ADDR>] ...` — run a distributed
    // actor node (shard 0, network-enabled).
    if args[1] == "node" {
        if let Err(e) = run_node_cmd(&args[2..]) {
            print_error(&e, true);
            std::process::exit(exit_code(&e));
        }
        return;
    }

    if args[1] == "agent" {
        #[cfg(feature = "ai-runtime")]
        {
            if let Err(e) = nulang::agent::commands::run(&args[2..]) {
                print_error(&e, true);
                std::process::exit(exit_code(&e));
            }
            return;
        }
        #[cfg(not(feature = "ai-runtime"))]
        {
            eprintln!("error: `nulang agent` requires the ai-runtime feature");
            std::process::exit(1);
        }
    }

    if args[1] == "nula" {
        if let Err(e) = nulang::package::commands::run(&args[2..]) {
            print_error(&e, true);
            std::process::exit(exit_code(&e));
        }
        return;
    }

    // Parse arguments
    let mut opts = Options::default();
    let mut positional = Vec::new();
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "-r" | "--repl" => opts.repl = true,
            "-e" | "--eval" => {
                if i + 1 < args.len() {
                    opts.eval_code = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --eval requires a code argument");
                    std::process::exit(1);
                }
            }
            "-c" | "--check" => {
                if i + 1 < args.len() {
                    opts.check_file = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --check requires a file argument");
                    std::process::exit(1);
                }
            }
            "--version" | "-V" => {
                println!("nulang {}", VERSION);
                println!(
                    "language {}",
                    nulang::format::constants::LANGUAGE_VERSION_STR
                );
                return;
            }
            "--language-version" => {
                println!("{}", nulang::format::constants::LANGUAGE_VERSION_STR);
                return;
            }
            "--lsp" => opts.lsp = true,
            "--dap" => opts.dap = true,
            "--doc" => opts.doc = true,
            "--backend" => {
                if i + 1 < args.len() {
                    opts.backend = args[i + 1].clone();
                    i += 1;
                } else {
                    eprintln!(
                        "Error: --backend requires an argument (bytecode | native{})",
                        if cfg!(feature = "wasm-backend") {
                            " | wasm | wasm-run | wasm-aot"
                        } else {
                            ""
                        }
                    );
                    std::process::exit(1);
                }
            }
            "--target" => {
                if i + 1 < args.len() {
                    opts.target = args[i + 1].clone();
                    i += 1;
                } else {
                    eprintln!("Error: --target requires an argument (native | ptx | riscv64)");
                    std::process::exit(1);
                }
            }
            "--out" => {
                if i + 1 < args.len() {
                    opts.out_file = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --out requires a file path argument");
                    std::process::exit(1);
                }
            }
            "--store" => {
                if i + 1 < args.len() {
                    opts.store_path = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --store requires a directory path argument");
                    std::process::exit(1);
                }
            }
            "--ffi-sandbox" => opts.ffi_sandbox = true,
            "--iso-arena" => opts.iso_arena = true,
            "--ffi-allow" => {
                if i + 1 < args.len() {
                    opts.ffi_allow.push(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --ffi-allow requires a library name or path argument");
                    std::process::exit(1);
                }
            }
            "--with" => {
                if i + 1 < args.len() {
                    for cap in args[i + 1].split(',') {
                        let cap = cap.trim();
                        if !cap.is_empty() {
                            opts.with_capabilities.push(cap.to_string());
                        }
                    }
                    i += 1;
                } else {
                    eprintln!(
                        "Error: --with requires a comma-separated capability list (fs,net,os)"
                    );
                    std::process::exit(1);
                }
            }
            "--" => {
                // Everything after -- is a positional argument.
                for arg in args[i + 1..].iter() {
                    positional.push(arg.to_string());
                }
                break;
            }
            "--emit-stdlib-docs" => {
                if i + 1 < args.len() {
                    opts.emit_stdlib_docs = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --emit-stdlib-docs requires a directory argument");
                    std::process::exit(1);
                }
            }
            "--emit-signals" => {
                if i + 1 < args.len() {
                    opts.emit_signals = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --emit-signals requires a file path argument");
                    std::process::exit(1);
                }
            }
            "--rewrite-signals" => {
                if i + 1 < args.len() {
                    opts.rewrite_signals = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --rewrite-signals requires a file path argument");
                    std::process::exit(1);
                }
            }
            "init" => {
                if i + 1 < args.len() {
                    opts.init = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("init requires a name");
                    std::process::exit(1);
                }
            }
            "--watch" => {
                if i + 1 < args.len() {
                    opts.watch = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("--watch requires a file");
                    std::process::exit(1);
                }
            }
            "--explain" => {
                if i + 1 < args.len() {
                    opts.explain = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("--explain requires a code");
                    std::process::exit(1);
                }
            }
            "-v" | "--verbose" => opts.verbose = true,
            "--all-errors" => opts.all_errors = true,
            "--json" => opts.json = true,
            "--deny-warnings" => opts.deny_warnings = true,
            "--metrics-port" => {
                if i + 1 < args.len() {
                    match args[i + 1].parse::<u16>() {
                        Ok(port) => opts.metrics_port = Some(port),
                        Err(_) => {
                            eprintln!("Error: --metrics-port requires a valid port number");
                            std::process::exit(1);
                        }
                    }
                } else {
                    eprintln!("Error: --metrics-port requires a port number");
                    std::process::exit(1);
                }
            }
            "--color" => {
                if i + 1 < args.len() {
                    let val = args[i + 1].clone();
                    if val != "auto" && val != "always" && val != "never" {
                        eprintln!(
                            "Error: --color must be 'auto', 'always', or 'never', got '{}'",
                            val
                        );
                        std::process::exit(1);
                    }
                    opts.color = val;
                    i += 1;
                } else {
                    eprintln!("Error: --color requires an argument (auto|always|never)");
                    std::process::exit(1);
                }
            }
            "--emit-nbc" => opts.emit_nbc = true,
            "--verify" => {
                if i + 1 < args.len() {
                    opts.verify_source = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --verify requires a source file path argument");
                    std::process::exit(1);
                }
            }
            "--bench" => {
                opts.bench_count = Some(10); // default
                if i + 1 < args.len() {
                    if let Ok(n) = args[i + 1].parse::<usize>() {
                        if n > 0 {
                            opts.bench_count = Some(n);
                            i += 1;
                        }
                    }
                }
            }
            "-h" | "--help" => {
                print_help();
                return;
            }
            arg if arg.starts_with('-') => {
                let known: &[&str] = &[
                    "--repl",
                    "--eval",
                    "--check",
                    "--lsp",
                    "--dap",
                    "--doc",
                    "--backend",
                    "--out",
                    "--emit-nbc",
                    "--verify",
                    "--bench",
                    "--json",
                    "--store",
                    "--version",
                    "--verbose",
                    "--color",
                    "--help",
                    "--emit-stdlib-docs",
                    "--emit-signals",
                    "--rewrite-signals",
                    "-r",
                    "-e",
                    "-c",
                    "-V",
                    "-v",
                    "-h",
                ];
                let suggestion = known
                    .iter()
                    .min_by_key(|k| levenshtein_distance(arg, k))
                    .filter(|k| levenshtein_distance(arg, k) <= 3);
                eprint!("Error: Unknown option: {}", arg);
                if let Some(sug) = suggestion {
                    eprint!(". Did you mean '{}'?", sug);
                }
                eprintln!();
                eprintln!("Run with --help for usage information.");
                std::process::exit(1);
            }
            arg => positional.push(arg.to_string()),
        }
        i += 1;
    }

    // Resolve color mode once after all args are parsed.
    let use_color = color_enabled(&opts);
    // Wave D4: --iso-arena enables the VM's per-activation arena path for
    // every VM created in this process (the VM also honors the env var
    // directly; set_var keeps runtimes that construct VMs internally in
    // sync without threading a flag through every constructor).
    if opts.iso_arena {
        std::env::set_var("NULANG_ISO_ARENA", "1");
    }

    // Apply FFI policy
    if opts.ffi_sandbox {
        use nulang::ffi::native::FfiPolicy;
        use std::collections::HashSet;

        let allowed = opts.ffi_allow.clone().into_iter().collect::<HashSet<_>>();
        let mut reg = nulang::ffi::native::FFI_REGISTRY
            .get_or_init(|| std::sync::Mutex::new(nulang::ffi::native::FfiRegistry::new()))
            .lock()
            .unwrap();
        reg.set_policy(FfiPolicy::Allowlist(allowed));
    }

    if opts.doc {
        let root = match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("Error: Cannot determine current directory: {}", e);
                std::process::exit(1);
            }
        };
        match nulang::docgen::write_project_docs(&root) {
            Ok(path) => println!("Wrote {}", path.display()),
            Err(e) => {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
        }
        return;
    }

    if let Some(dir) = opts.emit_stdlib_docs {
        match emit_stdlib_docs(&dir) {
            Ok(()) => println!("Stdlib docs written to {}", dir),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if opts.lsp {
        #[cfg(feature = "lsp")]
        {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async { nulang::lsp::run_lsp_server().await });
            return;
        }
        #[cfg(not(feature = "lsp"))]
        {
            eprintln!("Error: this build was compiled without the 'lsp' feature.");
            std::process::exit(1);
        }
    }
    if opts.dap {
        nulang::dap::run_dap_server();
        return;
    }

    if let Some(n) = &opts.init {
        let d = std::path::PathBuf::from(n);
        if d.exists() {
            eprintln!("dir exists");
            std::process::exit(1);
        }
        std::fs::create_dir_all(&d).unwrap();
        let m = d.join("main.nula");
        std::fs::write(
            &m,
            format!(
                "// {} - Nulang experiment\nperform IO.print(\"Hello!\")\n",
                n
            ),
        )
        .unwrap();
        println!("Created {}", m.display());
        return;
    }
    if let Some(c) = &opts.explain {
        use nulang::types::ErrorCode;
        // Accept both the legacy flat codes (E001..E012) and the stable
        // category-scoped codes (E01xx..E05xx; see docs/ERROR_CODES.md).
        let e = match c.to_uppercase().as_str() {
            "E001" | "E0103" => ErrorCode::E001UnclosedDelimiter,
            "E002" | "E0202" => ErrorCode::E002UnboundVariable,
            "E003" | "E0201" => ErrorCode::E003TypeMismatch,
            "E004" | "E0301" => ErrorCode::E004MissingEffect,
            "E005" | "E0401" => ErrorCode::E005SendabilityViolation,
            "E006" | "E0402" => ErrorCode::E006LinearUseAfterConsume,
            "E007" | "E0203" => ErrorCode::E007InfiniteType,
            "E008" | "E0204" => ErrorCode::E008FieldNotFound,
            "E009" | "E0205" => ErrorCode::E009WrongArity,
            "E010" | "E0206" => ErrorCode::E010MatchNoArms,
            "E011" | "E0503" => ErrorCode::E011StepLimitExceeded,
            "E012" | "E0302" => ErrorCode::E012UnhandledEffect,
            "E013" | "E0208" => ErrorCode::E013FfiBoundaryViolation,
            _ => {
                eprintln!("Unknown: {}", c);
                std::process::exit(1);
            }
        };
        println!("{}", e.explain());
        return;
    }
    if let Some(p) = &opts.watch {
        let p = p.clone();
        let v = opts.verbose;
        let b = opts.backend.clone();
        let uc = color_enabled(&opts);
        eprintln!("Watching {}...", p);
        let mut lm = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok());
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let cm = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok());
            if cm != lm {
                lm = cm;
                eprintln!("\n--- {} ---", p);
                if let Ok(s) = std::fs::read_to_string(&p) {
                    if let Err(e) = run_source(
                        &s,
                        Some(&p),
                        v,
                        &b,
                        None,
                        None,
                        &opts.target,
                        &opts.with_capabilities,
                        opts.store_path.as_deref(),
                        opts.deny_warnings,
                    ) {
                        print_error(&e, uc);
                    }
                }
            }
        }
    }
    if opts.repl {
        let mut repl = Repl::new();
        repl.run();
        return;
    }
    if let Some(code) = opts.eval_code {
        if opts.emit_nbc {
            let out = opts
                .out_file
                .clone()
                .unwrap_or_else(|| "out.nbc".to_string());
            if let Err(e) = compile_source_to_nbc(
                &code,
                &out,
                opts.rewrite_signals.as_deref(),
                &opts.with_capabilities,
                opts.deny_warnings,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
            return;
        }
        if let Some(n) = opts.bench_count {
            if let Err(e) = run_bench(
                || {
                    run_source(
                        &code,
                        None,
                        opts.verbose,
                        &opts.backend,
                        opts.out_file.as_deref(),
                        opts.metrics_port,
                        &opts.target,
                        &opts.with_capabilities,
                        opts.store_path.as_deref(),
                        opts.deny_warnings,
                    )
                },
                n,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
        } else {
            if let Err(e) = run_source(
                &code,
                None,
                opts.verbose,
                &opts.backend,
                opts.out_file.as_deref(),
                opts.metrics_port,
                &opts.target,
                &opts.with_capabilities,
                opts.store_path.as_deref(),
                opts.deny_warnings,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
        }
    }
    if let Some(path) = opts.check_file {
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error: Cannot read file '{}': {}", path, e);
                std::process::exit(1);
            }
        };
        if let Err(e) = check_source(
            &source,
            Some(&path),
            opts.verbose,
            opts.all_errors,
            &opts.with_capabilities,
            opts.deny_warnings,
        ) {
            let code = exit_code(&e);
            if opts.json {
                // Machine-readable mode: the JSON report is the ONLY output on
                // stdout; nothing human-rendered is printed.
                let diags = if opts.all_errors {
                    let all = collect_all_frontend_errors(&source, Some(&path));
                    if all.is_empty() {
                        nulang::json_diagnostics::diagnostics_from_error(&e)
                    } else {
                        all.iter()
                            .flat_map(nulang::json_diagnostics::diagnostics_from_error)
                            .collect()
                    }
                } else {
                    nulang::json_diagnostics::diagnostics_from_error(&e)
                };
                let report =
                    nulang::json_diagnostics::JsonReport::new("check", Some(path.clone()), diags);
                print!("{}", report.to_json_string());
            } else if opts.all_errors {
                let all = collect_all_frontend_errors(&source, Some(&path));
                if all.is_empty() {
                    print_error(&e, use_color);
                } else {
                    for err in &all {
                        print_error(err, use_color);
                    }
                }
            } else {
                print_error(&e, use_color);
            }
            std::process::exit(code);
        }
        if opts.json {
            let report =
                nulang::json_diagnostics::JsonReport::new("check", Some(path.clone()), Vec::new());
            print!("{}", report.to_json_string());
        } else {
            println!("Type check passed.");
        }
        return;
    }

    // Run a source file, or a pre-compiled `.nbc` artifact.
    if !positional.is_empty() {
        let path = &positional[0];

        // A `.nbc` artifact: load and run directly without invoking the
        // compiler. This is the durable-distribution path — a `.nbc` minted
        // in 2026 runs on any conforming runtime in 2126.
        if path.ends_with(".nbc") {
            if let Err(e) = run_nbc_file(path, opts.verify_source.as_deref()) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
            return;
        }

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error: Cannot read file '{}': {}", path, e);
                std::process::exit(1);
            }
        };

        // `--emit-signals`: analyze the module and write the signal graph JSON.
        if let Some(out) = opts.emit_signals.as_ref() {
            match run_frontend(
                &source,
                Some(path),
                opts.verbose,
                &opts.with_capabilities,
                opts.deny_warnings,
            ) {
                Ok((ast, _)) => {
                    let mut checker = nulang::effect_checker::EffectChecker::new();
                    checker.set_resource_grants(&opts.with_capabilities);
                    let _ = checker.check_module(&ast.decls);
                    let graph = nulang::web::reactivity::analyze_module(&ast, Some(&checker));
                    if let Err(e) = std::fs::write(out, graph.to_json()) {
                        eprintln!("Error: Cannot write signal graph '{}': {}", out, e);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    print_error(&e, use_color);
                    std::process::exit(exit_code(&e));
                }
            }
            return;
        }

        // `--emit-nbc`: compile to a `.nbc` artifact and write it, don't run.
        if opts.emit_nbc {
            let out = opts.out_file.clone().unwrap_or_else(|| {
                // foo.nula -> foo.nbc; anything else -> <path>.nbc
                if let Some(stem) = path.strip_suffix(".nula") {
                    format!("{stem}.nbc")
                } else {
                    format!("{path}.nbc")
                }
            });
            if let Err(e) = compile_source_to_nbc(
                &source,
                &out,
                opts.rewrite_signals.as_deref(),
                &opts.with_capabilities,
                opts.deny_warnings,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
            return;
        }
        if let Some(n) = opts.bench_count {
            let verbose = opts.verbose;
            let backend = &opts.backend;
            let out_file = opts.out_file.as_deref();
            if let Err(e) = run_bench(
                || {
                    run_source(
                        &source,
                        Some(path),
                        verbose,
                        backend,
                        out_file,
                        opts.metrics_port,
                        &opts.target,
                        &opts.with_capabilities,
                        opts.store_path.as_deref(),
                        opts.deny_warnings,
                    )
                },
                n,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
        } else {
            if let Err(e) = run_source(
                &source,
                Some(path),
                opts.verbose,
                &opts.backend,
                opts.out_file.as_deref(),
                opts.metrics_port,
                &opts.target,
                &opts.with_capabilities,
                opts.store_path.as_deref(),
                opts.deny_warnings,
            ) {
                print_error(&e, use_color);
                std::process::exit(exit_code(&e));
            }
            // Metrics node: the metrics server runs on a background thread and
            // would die with the process once the program finishes. When
            // `--metrics-port` is set, stay alive so /metrics keeps serving
            // the final snapshot published by run_with_runtime (same contract
            // as `registry serve`). Stop with Ctrl-C.
            if let Some(port) = opts.metrics_port {
                eprintln!("Program finished; serving /metrics on :{port} (Ctrl-C to stop)");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        return;
    }
    // No arguments and no options: if stdin is piped, execute as script.
    if !std::io::stdin().is_terminal() {
        let mut source = String::new();
        std::io::stdin()
            .read_to_string(&mut source)
            .expect("Failed to read stdin");
        if let Err(e) = run_source(
            &source,
            None,
            opts.verbose,
            &opts.backend,
            opts.out_file.as_deref(),
            opts.metrics_port,
            &opts.target,
            &opts.with_capabilities,
            opts.store_path.as_deref(),
            opts.deny_warnings,
        ) {
            print_error(&e, use_color);
            std::process::exit(exit_code(&e));
        }
        return;
    }
    let mut repl = Repl::new();
    repl.run();
}

struct Options {
    repl: bool,
    eval_code: Option<String>,
    check_file: Option<String>,
    lsp: bool,
    dap: bool,
    doc: bool,
    verbose: bool,
    backend: String,
    out_file: Option<String>,
    /// Compile the input to a `.nbc` artifact and write it, don't run.
    emit_nbc: bool,
    /// When running a `.nbc` artifact, verify its recorded source hash against
    /// this source file before executing. Refuses on mismatch.
    verify_source: Option<String>,
    /// Output directory for --emit-stdlib-docs.
    emit_stdlib_docs: Option<String>,
    /// Output file for the compile-time signal graph (`.nula/dist/app.signals.json`).
    emit_signals: Option<String>,
    /// Output file for the client-side signal micro-runtime (`.nula/dist/app.client.js`).
    rewrite_signals: Option<String>,
    /// Color mode: "auto" (default), "always", or "never".
    color: String,
    init: Option<String>,
    watch: Option<String>,
    explain: Option<String>,
    all_errors: bool,
    /// Emit machine-readable JSON diagnostics on stdout (see
    /// `nulang::json_diagnostics` for the schema).
    json: bool,
    bench_count: Option<usize>,
    /// Start a Prometheus-format metrics server on this port.
    metrics_port: Option<u16>,
    ffi_sandbox: bool,
    /// Wave D4: enable the per-activation iso-arena allocation path in the
    /// bytecode VM (same as `NULANG_ISO_ARENA=1`). Default off.
    iso_arena: bool,
    ffi_allow: Vec<String>,
    /// Resource-capability grants for `--with=` (fs, net, os). Empty = no
    /// gate (standalone programs run with full access).
    with_capabilities: Vec<String>,
    /// Target ISA for AOT compilation: native (default), ptx, riscv64
    target: String,
    /// Durable store directory for programs that declare durable/persistent
    /// entities. `None` = resolve at run time: `NULANG_STORE_PATH` env var,
    /// else `.nulang/store/`. Only consulted when the program declares
    /// durable entities; other programs keep the in-memory store.
    store_path: Option<String>,
    /// Escalate warnings (e.g. RFC 0015 deprecations) to a hard error.
    deny_warnings: bool,
}
impl Default for Options {
    fn default() -> Self {
        Options {
            repl: false,
            eval_code: None,
            check_file: None,
            lsp: false,
            dap: false,
            doc: false,
            verbose: false,
            backend: "bytecode".to_string(),
            out_file: None,
            emit_nbc: false,
            verify_source: None,
            emit_stdlib_docs: None,
            emit_signals: None,
            rewrite_signals: None,
            color: "auto".to_string(),
            init: None,
            watch: None,
            explain: None,
            all_errors: false,
            json: false,
            bench_count: None,
            metrics_port: None,
            ffi_sandbox: false,
            iso_arena: false,
            ffi_allow: Vec::new(),
            with_capabilities: Vec::new(),
            target: "native".to_string(),
            store_path: None,
            deny_warnings: false,
        }
    }
}
fn print_help() {
    println!("Usage: nulang [OPTIONS] <FILE>");
    println!("       nulang --repl");
    println!("       nulang --eval <CODE>");
    println!("       nulang --check <FILE>");
    println!("       nulang --lsp");
    println!("       nulang --dap");
    println!("       nulang fmt [--check] [<file>]");
    println!("       nulang node --listen <ADDR> [--seed <ADDR>] [--expected-nodes <N>]");
    println!("       nulang --doc");
    println!();
    println!("Options:");
    println!("  -r, --repl       Start interactive REPL");
    println!("  -e, --eval       Evaluate a code string");
    println!("  -c, --check      Type-check a file (don't run)");
    println!("  --doc            Generate Markdown API docs (docs/api.md)");
    println!("  --emit-stdlib-docs <dir>  Generate per-effect stdlib Markdown docs into <dir>");
    println!("  --lsp            Start Language Server (stdio)");
    println!("  --dap            Start Debug Adapter (stdio; program via launch request)");
    print!("  --backend <b>    Backend: bytecode (default) | native | core-vm");
    if cfg!(feature = "wasm-backend") {
        print!(" | wasm | wasm-run | wasm-aot");
    }
    println!();
    println!("                   core-vm: frozen Core interpreter (Stage 3 bootstrap)");
    println!("                   native: pure-functional subset only (no effects,");
    println!("                   actors, or FFI — errors name the unsupported");
    println!("                   construct; use bytecode for full-language programs)");
    if cfg!(feature = "wasm-backend") {
        println!("                   wasm*: IO.print/read only (no user-defined effect");
        println!("                   handlers, no actor mailbox)");
    }
    if cfg!(feature = "wasmfx-backend") {
        println!("                   wasmfx*: suspending effects lower to WasmFX stack");
        println!("                   switching (LLM.ask, Signal.wait, ReceiveWait)");
    }
    println!("  --target <t>     Target ISA for native backend: native (default) | ptx | riscv64");
    if cfg!(feature = "wasm-backend") {
        println!("  --out <file>     Output file for WASM backends (default: out.wasm)");
    }
    println!("  --out <file>     Output path for --emit-nbc (default: <FILE> with .nbc extension)");
    println!("  <FILE>.nbc       Run a pre-compiled .nbc artifact directly (no compiler invoked)");
    println!(
        "  --verify <src>   When running a .nbc artifact, verify its source hash against <src>"
    );
    println!(
        "  nula <cmd>       Package manager (new, init, build, build-wasm, test, run, add, remove, watch, doc, list, clean)"
    );
    println!("  --version, -V    Print version and exit");
    println!("  init <name>      Scaffold experiment");
    println!("  --watch <file>   Re-run on changes");
    println!("  --explain <CODE> Error code help");
    println!("  --all-errors     Report all type errors (not just the first)");
    println!("  --json           Emit machine-readable JSON diagnostics on stdout");
    println!("  --deny-warnings  Treat warnings (e.g. RFC 0015 deprecations) as errors");
    println!("  --bench [N]      Benchmark: run N times (default 10), print timing stats");
    println!("  fmt [--check] [<file>]  Format file(s); no file → all src/**/*.nula");
    println!("  -v, --verbose    Show bytecode and AST");
    println!("  --metrics-port <N>  Start Prometheus metrics server on port N");
    println!("  --emit-signals <file> Emit signal graph JSON for the web framework");
    println!("  --rewrite-signals <file> Rewrite HTML for signals and emit client JS");
    println!("  --store <dir>    Durable store directory for programs declaring durable");
    println!("                   entities (default: $NULANG_STORE_PATH or .nulang/store/)");
    println!("  --color auto|always|never  Colorize error output (default: auto)");
    println!("  -h, --help       Show this help message");
}

/// Generate per-effect stdlib Markdown docs into the given directory.
fn emit_stdlib_docs(dir: &str) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;

    let out_dir = PathBuf::from(dir);
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Cannot create directory '{}': {}", dir, e))?;

    let stdlib = StdLib::new();
    let mut by_effect: BTreeMap<&str, Vec<&nulang::stdlib::BuiltinOp>> = BTreeMap::new();
    for op in stdlib.ops() {
        by_effect.entry(op.effect).or_default().push(op);
    }

    for (&effect_name, ops) in &by_effect {
        // Build a per-effect Starlight docs page.
        // These files are auto-generated — never edit them by hand.
        // Source of truth: `src/stdlib.rs` (the `StdLib::new()` registry).
        let mut page = String::new();
        page.push_str("---\n");
        page.push_str(&format!("title: \"{} Effect\"\n", effect_name));
        page.push_str(&format!(
            "description: \"Built-in {} effect operations (auto-generated from src/stdlib.rs)\"\n",
            effect_name
        ));
        page.push_str("sidebar:\n");
        page.push_str(&format!("  label: \"{}\"\n", effect_name));
        page.push_str("editUrl: false\n");
        page.push_str("---\n\n");
        page.push_str("> **This page is auto-generated from `src/stdlib.rs`.**\n");
        page.push_str(
            "> Do not edit it by hand — your changes will be overwritten on the next CI run.\n",
        );
        page.push_str("> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.\n\n");
        page.push_str(&format!("# {} Effect\n\n", effect_name));
        page.push_str(&format!(
            "The `{}` effect provides the following built-in operations, wired into the VM and runtime.\n\n",
            effect_name
        ));
        page.push_str("| Operation | Signature | Description |\n");
        page.push_str("|-----------|-----------|-------------|\n");
        for op in ops {
            page.push_str(&format!(
                "| `{}` | `{}` | {} |\n",
                op.name,
                op.signature.replace('|', "\\|"),
                op.description
            ));
        }
        page.push_str(&format!(
            "\n_Implementation site: {}_\n",
            match ops.first().map(|o| o.implemented_in) {
                Some(nulang::stdlib::ImplSite::StandaloneVm) => "Standalone VM",
                Some(nulang::stdlib::ImplSite::RuntimeHost) => "Runtime Host",
                None => "Unknown",
            }
        ));

        let filename = out_dir.join(format!("{}.md", effect_name.to_lowercase()));
        let mut file = fs::File::create(&filename)
            .map_err(|e| format!("Cannot create '{}': {}", filename.display(), e))?;
        file.write_all(page.as_bytes())
            .map_err(|e| format!("Cannot write '{}': {}", filename.display(), e))?;
    }
    Ok(())
}

/// Run a distributed Nulang node: parse arguments, create a Runtime,
/// enable distribution, join a seed cluster if requested, and run forever.
#[cfg(feature = "tcp")]
fn run_node_cmd(args: &[String]) -> NuResult<()> {
    let mut listen_addr = "127.0.0.1:9000".to_string();
    let mut seed_addr: Option<String> = None;
    let mut expected_nodes: Option<usize> = None;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    let mut tls_ca: Option<String> = None;
    let mut plaintext = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                if i + 1 < args.len() {
                    listen_addr = args[i + 1].clone();
                    i += 1;
                } else {
                    eprintln!("Error: --listen requires an address argument");
                    std::process::exit(1);
                }
            }
            "--seed" => {
                if i + 1 < args.len() {
                    seed_addr = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --seed requires an address argument");
                    std::process::exit(1);
                }
            }
            "--expected-nodes" => {
                if i + 1 < args.len() {
                    expected_nodes = Some(args[i + 1].parse().unwrap_or(1));
                    i += 1;
                } else {
                    eprintln!("Error: --expected-nodes requires a count argument");
                    std::process::exit(1);
                }
            }
            "--tls-cert" => {
                if i + 1 < args.len() {
                    tls_cert = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --tls-cert requires a path argument");
                    std::process::exit(1);
                }
            }
            "--tls-key" => {
                if i + 1 < args.len() {
                    tls_key = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --tls-key requires a path argument");
                    std::process::exit(1);
                }
            }
            "--tls-ca" => {
                if i + 1 < args.len() {
                    tls_ca = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    eprintln!("Error: --tls-ca requires a path argument");
                    std::process::exit(1);
                }
            }
            "--plaintext" => plaintext = true,
            "-h" | "--help" => {
                println!("Usage: nulang node [OPTIONS]");
                println!();
                println!("Options:");
                println!("  --listen <ADDR>       Bind address (default: 127.0.0.1:9000)");
                println!("  --seed <ADDR>         Seed node to join (optional)");
                println!("  --expected-nodes <N>  Expected cluster size for split-brain quorum");
                println!("  --tls-cert <PATH>     Server certificate (PEM)");
                println!("  --tls-key <PATH>      Server private key (PEM)");
                println!("  --tls-ca <PATH>       CA certificate for mutual TLS");
                println!("  --plaintext           Disable TLS (insecure, dev only)");
                println!("  -h, --help            Show this help message");
                return Ok(());
            }
            arg => {
                eprintln!("Error: Unknown node option: {}", arg);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let bind_addr: std::net::SocketAddr =
        listen_addr.parse().map_err(|e| NuError::RuntimeError {
            msg: format!("invalid bind address: {e}"),
            span: Span::default(),
        })?;

    let tls_config = if plaintext {
        nulang::runtime::TlsConfig::PlaintextInsecure
    } else {
        let cert = tls_cert
            .as_ref()
            .map(|p| std::fs::read_to_string(p).ok())
            .flatten()
            .unwrap_or_default();
        let key = tls_key
            .as_ref()
            .map(|p| std::fs::read_to_string(p).ok())
            .flatten()
            .unwrap_or_default();
        let ca = tls_ca
            .as_ref()
            .map(|p| std::fs::read_to_string(p).ok())
            .flatten()
            .unwrap_or_default();
        if cert.is_empty() && key.is_empty() && ca.is_empty() {
            eprintln!("Warning: no TLS certificates provided; using plaintext transport");
            nulang::runtime::TlsConfig::PlaintextInsecure
        } else {
            nulang::runtime::TlsConfig::MutualTls {
                ca_cert_pem: ca.into_bytes(),
                server_cert_pem: cert.into_bytes(),
                server_key_pem: key.into_bytes(),
                server_name: None,
            }
        }
    };

    let mut runtime = nulang::runtime::Runtime::new();

    if let Some(n) = expected_nodes {
        runtime.cluster_config = nulang::runtime::ClusterConfig {
            split_brain: nulang::runtime::SplitBrainConfig::StaticQuorum { expected_nodes: n },
            probe_interval: std::time::Duration::from_secs(5),
            ..Default::default()
        };
    }

    if let Err(e) = runtime.enable_distribution(bind_addr, tls_config) {
        return Err(NuError::RuntimeError {
            msg: format!("failed to enable distribution: {e}"),
            span: Span::default(),
        });
    }

    if let Some(seed) = seed_addr {
        let seed_socket: std::net::SocketAddr =
            seed.parse().map_err(|e| NuError::RuntimeError {
                msg: format!("invalid seed address: {e}"),
                span: Span::default(),
            })?;
        runtime.join_cluster(seed_socket);
    }

    eprintln!(
        "Nulang node listening on {} (node_id: {:?})",
        bind_addr, runtime.distributed.node_id
    );
    runtime.run_distributed_node();
    Ok(())
}

/// Run a distributed Nulang node.
///
/// Stub used when the `tcp` feature is disabled: real TCP distribution is
/// unavailable, so the node cannot start.
#[cfg(not(feature = "tcp"))]
fn run_node_cmd(_args: &[String]) -> NuResult<()> {
    Err(NuError::RuntimeError {
        msg: "the 'node' command requires the 'tcp' feature (build with --features tcp)"
            .to_string(),
        span: Span::default(),
    })
}

fn print_error(err: &NuError, use_color: bool) {
    // Use the canonical rich diagnostic renderer (ariadne source snippet when
    // a source map is installed, plain Rust-style fallback otherwise). The
    // rendered report already ends with a newline.
    eprint!("{}", nulang::diagnostic::format_diagnostic(err, use_color));
}

/// Resolve the `--color` flag against `is_terminal`.
fn color_enabled(opts: &Options) -> bool {
    match opts.color.as_str() {
        "always" => true,
        "never" => false,
        _ => std::io::stderr().is_terminal(),
    }
}

/// Map each error kind to a distinct exit code so tooling can
/// discriminate between syntax, type, runtime, and system errors.
fn exit_code(err: &NuError) -> i32 {
    match err {
        NuError::LexError { .. } => 2,
        NuError::ParseError { .. } => 3,
        NuError::TypeError { .. } => 4,
        NuError::EffectError { .. } => 5,
        NuError::CapError { .. } => 6,
        NuError::FFIError { .. } => 7,
        NuError::NotYetImplemented { .. } => 8,
        NuError::RuntimeError { .. } => 9,
        NuError::VMError { .. } => 10,
        NuError::Suspended(_) => 0, // Not an error — runtime handles suspensions
        NuError::PythonError { .. } => 11,
        NuError::PackageError { .. } => 12,
        NuError::Multiple(_) => 3, // Same as ParseError — accumulated parse errors
    }
}

// ---------------------------------------------------------------------------
// Benchmark helpers
// ---------------------------------------------------------------------------

/// Redirect stdout and stderr to /dev/null.
/// Returns saved file descriptors for later restoration.
#[cfg(unix)]
fn suppress_stdout_stderr() -> (i32, i32) {
    extern "C" {
        fn dup(oldfd: i32) -> i32;
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    let stdout_fd = std::io::stdout().as_raw_fd();
    let stderr_fd = std::io::stderr().as_raw_fd();
    let saved_out = unsafe { dup(stdout_fd) };
    let saved_err = unsafe { dup(stderr_fd) };
    if saved_out < 0 || saved_err < 0 {
        return (saved_out, saved_err);
    }
    if let Ok(null) = std::fs::File::open("/dev/null") {
        let null_fd = null.as_raw_fd();
        unsafe {
            dup2(null_fd, stdout_fd);
            dup2(null_fd, stderr_fd);
        }
    }
    (saved_out, saved_err)
}

#[cfg(unix)]
fn restore_stdout_stderr(saved_out: i32, saved_err: i32) {
    extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
        fn close(fd: i32) -> i32;
    }
    if saved_out >= 0 {
        unsafe {
            dup2(saved_out, 1);
            close(saved_out);
        }
    }
    if saved_err >= 0 {
        unsafe {
            dup2(saved_err, 2);
            close(saved_err);
        }
    }
}

/// Windows: no fd redirection — benchmark runs keep visible output.
#[cfg(not(unix))]
fn suppress_stdout_stderr() -> (i32, i32) {
    (0, 0)
}

#[cfg(not(unix))]
fn restore_stdout_stderr(_saved_out: i32, _saved_err: i32) {}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{:.1} ms", secs * 1000.0)
    } else {
        format!("{:.2} s", secs)
    }
}

fn print_bench_stats(times: &[std::time::Duration]) {
    let mut sorted: Vec<f64> = times.iter().map(|d| d.as_secs_f64()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let min = sorted[0];
    let max = sorted[n - 1];
    let mean = sorted.iter().sum::<f64>() / n as f64;
    let median = if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };
    println!();
    println!("runs: {}", n);
    println!(
        "min:    {}",
        format_duration(std::time::Duration::from_secs_f64(min))
    );
    println!(
        "mean:   {}",
        format_duration(std::time::Duration::from_secs_f64(mean))
    );
    println!(
        "median: {}",
        format_duration(std::time::Duration::from_secs_f64(median))
    );
    println!(
        "max:    {}",
        format_duration(std::time::Duration::from_secs_f64(max))
    );
}

/// Run a closure `n` times, measuring wall-clock duration of each run.
/// The first run's output is visible; subsequent runs are silenced.
fn run_bench<F: FnMut() -> NuResult<()>>(mut run: F, n: usize) -> NuResult<()> {
    let mut times: Vec<std::time::Duration> = Vec::with_capacity(n);

    // First run — visible output
    let t0 = Instant::now();
    run()?;
    times.push(t0.elapsed());

    // Remaining runs — suppress stdout/stderr for clean timing
    if n > 1 {
        let (saved_out, saved_err) = suppress_stdout_stderr();
        for _ in 1..n {
            let t0 = Instant::now();
            run()?;
            times.push(t0.elapsed());
        }
        restore_stdout_stderr(saved_out, saved_err);
    }

    // Flush any buffered output before stats
    let _ = std::io::stdout().flush();

    print_bench_stats(&times);
    Ok(())
}

/// Shared frontend: lex -> parse -> typecheck -> effect check -> capability
/// analysis. Returns the parsed module ready for compilation.
///
/// `file_path` is an optional display name for diagnostics (e.g. "main.nula").
#[instrument(level = "debug", skip(source))]
fn run_frontend(
    source: &str,
    file_path: Option<&str>,
    verbose: bool,
    with_capabilities: &[String],
    deny_warnings: bool,
) -> NuResult<(nulang::ast::AstModule, nulang::typechecker::TypeChecker)> {
    let ps = nulang::prelude_source::PRELUDE_SOURCE;
    let mut pl = Lexer::new(ps);
    nulang::types::set_source_map_with_file(ps, Some("<prelude>"));
    let pt = pl.lex()?;
    let mut pp = Parser::new(pt);
    let pa = pp.parse_module()?;
    let mut lexer = Lexer::new(source);
    nulang::types::set_source_map_with_file(source, file_path);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    let mut ast = parser.parse_module()?;
    // Surface non-fatal frontend warnings (e.g. RFC 0015 deprecations).
    // Warnings never fail compilation unless --deny-warnings is passed.
    let warnings = parser.take_warnings();
    if !warnings.is_empty() {
        let use_color = std::io::stderr().is_terminal();
        for w in &warnings {
            eprintln!("{}", nulang::diagnostic::format_warning(w, use_color));
        }
        if deny_warnings {
            return Err(nulang::types::NuError::parse_error(
                format!(
                    "aborting due to {} warning{} (--deny-warnings)",
                    warnings.len(),
                    if warnings.len() == 1 { "" } else { "s" }
                ),
                warnings[0].span,
            ));
        }
    }

    let pd: Vec<nulang::ast::Decl> = pa
        .decls
        .into_iter()
        .filter(|d| matches!(d, nulang::ast::Decl::VariantType { .. }))
        .collect();

    // 2b. Resolve imports — load and merge declarations from imported files.
    let mut stack = std::collections::HashSet::new();
    nulang::resolver::resolve_imports(
        &mut ast,
        std::path::Path::new(file_path.unwrap_or(".")),
        &mut stack,
    )?;

    // Prepend the prelude AFTER import resolution. `resolve_imports`
    // prepends imported declarations in front of `ast.decls`, so injecting
    // the prelude beforehand would leave imported function bodies ahead of
    // the `Option`/`Result` variant-type declarations — and the typechecker
    // binds variant constructors in declaration order, so an imported
    // function constructing `Ok`/`Some` would fail with
    // "Unbound variable: 'Ok'".
    let mut pd = pd;
    pd.append(&mut ast.decls);
    ast.decls = pd;
    if verbose {
        println!("=== AST ===");
        println!("{:#?}", ast);
        println!();
    }

    // 3. Type check
    let mut type_checker = TypeChecker::new();
    let module_type = type_checker.check_module(&ast)?;

    if verbose {
        println!("=== Inferred Type ===");
        println!("{}\n", type_to_string(&module_type));
    }

    // 4. Effect check. Two passes over module functions: first register a
    // name -> EffectRow map (declared rows where present, fixpoint-inferred
    // otherwise) so that call sites propagate callee effects, then enforce
    // declared rows. Bodies without a declared row are inference-only.
    // Nested `module {}` decls are flattened first (mirroring the
    // typechecker's flatten_decls).
    let flat_decls = nulang::effect_checker::flatten_decls(&ast.decls);
    let mut effect_checker = EffectChecker::new();
    effect_checker.set_resource_grants(with_capabilities);
    effect_checker.check_module(&ast.decls)?;
    for msg in &effect_checker.diagnostics {
        eprintln!("{}", msg);
    }

    // 4b. Web route parameter check. For every static `perform Web.route(...)`
    // call, verify that the handler reads exactly the parameters declared in
    // the path. This is conservative: unresolved handlers are skipped.
    let route_diagnostics = nulang::web::route_check::check_module(&ast);
    for diag in &route_diagnostics {
        eprintln!("route check: {}", diag.message);
    }
    if !route_diagnostics.is_empty() {
        return Err(NuError::TypeError {
            msg: format!(
                "{} route parameter mismatch(es) detected; see diagnostics above",
                route_diagnostics.len()
            ),
            span: Span::default(),
            expected_type: None,
            found_type: None,
            similar_names: None,
        });
    }

    // 5. Capability analysis over the same body set.
    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_body = |analyzer: &mut CapabilityAnalyzer,
                    ctx: &CapContext,
                    body: &nulang::ast::Expr|
     -> NuResult<()> { analyzer.infer_cap(ctx, body).map(|_| ()) };
    let seed_from_params = |ctx: &mut CapContext, params: &[nulang::ast::Param]| {
        for p in params {
            if let Some(c) = p.cap {
                *ctx = ctx.clone().with_binding(&p.name, c);
            }
        }
    };
    for decl in flat_decls.iter().copied() {
        match decl {
            nulang::ast::Decl::Function { body, params, .. } => {
                let mut ctx = CapContext::new();
                seed_from_params(&mut ctx, params);
                cap_body(&mut cap_analyzer, &ctx, body)?;
            }
            nulang::ast::Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for b in behaviors {
                    let mut ctx = CapContext::new();
                    seed_from_params(&mut ctx, &b.params);
                    cap_body(&mut cap_analyzer, &ctx, &b.body)?;
                }
                for (_, _, _, default) in state_fields {
                    let ctx = CapContext::new();
                    cap_body(&mut cap_analyzer, &ctx, default)?;
                }
                for (_, expr) in init {
                    let ctx = CapContext::new();
                    cap_body(&mut cap_analyzer, &ctx, expr)?;
                }
            }
            nulang::ast::Decl::Workflow {
                items, compensate, ..
            } => {
                for item in items {
                    let steps: &[nulang::ast::WorkflowStep] = match item {
                        nulang::ast::WorkflowItem::Step(s) => std::slice::from_ref(s),
                        nulang::ast::WorkflowItem::Parallel(steps) => steps,
                    };
                    for step in steps {
                        let ctx = CapContext::new();
                        cap_body(&mut cap_analyzer, &ctx, &step.body)?;
                        if let Some(comp) = &step.compensate {
                            cap_body(&mut cap_analyzer, &ctx, comp)?;
                        }
                    }
                }
                if let Some(comp) = compensate {
                    let ctx = CapContext::new();
                    cap_body(&mut cap_analyzer, &ctx, comp)?;
                }
            }
            _ => {}
        }
    }

    Ok((ast, type_checker))
}

#[cfg_attr(not(feature = "wasm-backend"), allow(unused_variables))]
fn run_source(
    source: &str,
    file_path: Option<&str>,
    verbose: bool,
    backend: &str,
    out_file: Option<&str>,
    metrics_port: Option<u16>,
    target: &str,
    with_capabilities: &[String],
    store_path: Option<&str>,
    deny_warnings: bool,
) -> NuResult<()> {
    let (ast, type_checker) =
        run_frontend(source, file_path, verbose, with_capabilities, deny_warnings)?;
    match backend {
        #[cfg(feature = "wasm-backend")]
        "wasm" => {
            let wasm_file = out_file.unwrap_or("out.wasm");
            let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
            let mir = nulang::mir_lower::lower_module(&hir)?;
            use nulang::backends::WasmBackend;
            let mut wasm_backend = nulang::backends::DefaultWasmBackend;
            let wasm_bytes = wasm_backend.compile(&mir, "main")?;
            if verbose {
                println!("=== WASM ({}) bytes ===", wasm_bytes.len());
            }
            std::fs::write(wasm_file, &wasm_bytes).map_err(|e| {
                nulang::types::NuError::VMError {
                    msg: format!("failed to write {}: {}", wasm_file, e),
                    span: Span::default(),
                }
            })?;
            println!("Wrote {} ({} bytes)", wasm_file, wasm_bytes.len());
            return Ok(());
        }
        #[cfg(feature = "wasm-backend")]
        "wasm-run" => {
            let wasm_file = out_file.unwrap_or("out.wasm");
            let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
            let mir = nulang::mir_lower::lower_module(&hir)?;
            use nulang::backends::WasmBackend;
            let mut wasm_backend = nulang::backends::DefaultWasmBackend;
            let wasm_bytes = wasm_backend.compile(&mir, "main")?;
            if verbose {
                println!("=== WASM ({}) bytes ===", wasm_bytes.len());
            }
            std::fs::write(wasm_file, &wasm_bytes).map_err(|e| {
                nulang::types::NuError::VMError {
                    msg: format!("failed to write {}: {}", wasm_file, e),
                    span: Span::default(),
                }
            })?;
            wasm_backend.run(&wasm_bytes)?;
            return Ok(());
        }
        #[cfg(feature = "wasm-backend")]
        "wasm-aot" => {
            let wasm_file = out_file.unwrap_or("out.wasm");
            let cwasm_file = wasm_file.replace(".wasm", ".cwasm");
            let cwasm_file = if cwasm_file == wasm_file {
                format!("{}.cwasm", wasm_file)
            } else {
                cwasm_file
            };
            let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
            let mir = nulang::mir_lower::lower_module(&hir)?;
            use nulang::backends::WasmBackend;
            let mut wasm_backend = nulang::backends::DefaultWasmBackend;
            let wasm_bytes = wasm_backend.compile(&mir, "main")?;
            if verbose {
                println!("=== WASM ({}) bytes ===", wasm_bytes.len());
            }
            std::fs::write(&wasm_file, &wasm_bytes).map_err(|e| {
                nulang::types::NuError::VMError {
                    msg: format!("failed to write {}: {}", wasm_file, e),
                    span: Span::default(),
                }
            })?;
            println!("Wrote {} ({} bytes)", wasm_file, wasm_bytes.len());
            nulang::wasm_runtime::aot_compile(&wasm_file, &cwasm_file)?;
            println!("Wrote {} (precompiled)", cwasm_file);
            return Ok(());
        }
        #[cfg(feature = "wasmfx-backend")]
        "wasmfx" => {
            let wasm_file = out_file.unwrap_or("out.wasm");
            let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
            let mir = nulang::mir_lower::lower_module(&hir)?;
            let mut wasmfx_backend = nulang::wasmfx_backend::WasmFxBackend::new();
            let wasm_bytes = wasmfx_backend.compile(&mir, "main")?;
            if verbose {
                println!("=== WASMFX ({}) bytes ===", wasm_bytes.len());
            }
            std::fs::write(wasm_file, &wasm_bytes).map_err(|e| {
                nulang::types::NuError::VMError {
                    msg: format!("failed to write {}: {}", wasm_file, e),
                    span: Span::default(),
                }
            })?;
            println!("Wrote {} ({} bytes, WasmFX)", wasm_file, wasm_bytes.len());
            return Ok(());
        }
        #[cfg(feature = "wasmfx-backend")]
        "wasmfx-run" => {
            let wasm_file = out_file.unwrap_or("out.wasm");
            let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
            let mir = nulang::mir_lower::lower_module(&hir)?;
            let mut wasmfx_backend = nulang::wasmfx_backend::WasmFxBackend::new();
            let wasm_bytes = wasmfx_backend.compile(&mir, "main")?;
            if verbose {
                println!("=== WASMFX ({}) bytes ===", wasm_bytes.len());
            }
            std::fs::write(wasm_file, &wasm_bytes).map_err(|e| {
                nulang::types::NuError::VMError {
                    msg: format!("failed to write {}: {}", wasm_file, e),
                    span: Span::default(),
                }
            })?;
            let mut runtime = nulang::wasmfx_runtime::WasmFxRuntime::new(&wasm_bytes)?;
            let result = runtime.run()?;
            let result_str = result.to_string_repr();
            if !result_str.is_empty() && result_str != "unit" && result_str != "()" {
                println!("{}", result_str);
            }
            return Ok(());
        }
        #[cfg(not(feature = "wasmfx-backend"))]
        "wasmfx" | "wasmfx-run" => {
            return Err(nulang::types::NuError::VMError {
                msg: "wasmfx backend not compiled in. Rebuild with --features wasmfx-backend"
                    .into(),
                span: Span::default(),
            })
        }
        #[cfg(not(feature = "wasm-backend"))]
        "wasm" | "wasm-run" | "wasm-aot" => Err(nulang::types::NuError::VMError {
            msg: "wasm backend not compiled in (enable 'wasm-backend' feature)".into(),
            span: Span::default(),
        }),
        "native" => {
            let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
            let mir = nulang::mir_lower::lower_module(&hir)?;
            if verbose {
                println!("=== AOT native compilation (target: {}) ===", target);
                for func in &mir.functions {
                    println!(
                        "  fn {} ({} locals, {} blocks)",
                        func.name,
                        func.locals.len(),
                        func.blocks.len()
                    );
                }
            }
            let aot_module = nulang::aot::AotModule::compile_for_target(&mir, target)?;

            // If --out is specified, write assembly to file
            if let Some(out_file) = out_file {
                let assembly = aot_module.emit_assembly();
                std::fs::write(out_file, assembly).map_err(|e| {
                    nulang::types::NuError::VMError {
                        msg: format!("failed to write assembly to {}: {}", out_file, e),
                        span: Span::default(),
                    }
                })?;
                println!("Wrote assembly to {}", out_file);
                return Ok(());
            }

            // Modules that declare actors need a real Runtime: spawn/send must
            // create live actors and the scheduler must drain their mailboxes.
            // Pure modules can use the synchronous standalone runner.
            let has_actors = !mir.behaviors.is_empty();
            let result_raw = if has_actors {
                let mut rt = nulang::runtime::Runtime::new();
                let has_durable = ast.decls.iter().any(|d| {
                    matches!(
                        d,
                        nulang::ast::Decl::Actor {
                            persistent: true,
                            ..
                        }
                    )
                }) || matches!(
                    ast.decls.iter().find(|d| matches!(d, nulang::ast::Decl::Actor { .. })),
                    Some(nulang::ast::Decl::Actor { state_fields, .. })
                        if state_fields.iter().any(|(_, model, _, _)| matches!(
                            model,
                            nulang::ast::StateModel::Durable | nulang::ast::StateModel::EventSourced
                        ))
                );
                let store_dir = if has_durable {
                    Some(
                        store_path
                            .map(|s| s.to_string())
                            .or_else(|| std::env::var("NULANG_STORE_PATH").ok())
                            .unwrap_or_else(|| ".nulang/store".to_string()),
                    )
                } else {
                    None
                };
                if let Some(dir) = store_dir.as_deref() {
                    install_file_store(&mut rt, dir)?;
                }
                if let Some(port) = metrics_port {
                    let _ = rt.enable_metrics_server(port);
                }
                let raw = aot_module.run_in_runtime(&mut rt)?;
                let failures = rt.workflow_failures();
                if !failures.is_empty() {
                    for (step_name, error) in &failures {
                        eprintln!("workflow step '{}' failed: {}", step_name, error);
                    }
                    std::process::exit(1);
                }
                if verbose {
                    let snap = rt.metrics_snapshot();
                    match serde_json::to_string(&snap) {
                        Ok(json) => eprintln!("[metrics] {}", json),
                        Err(_) => eprintln!("[metrics] <serialization error>"),
                    }
                    eprintln!("{}", rt.render_topology());
                }
                rt.publish_metrics();
                raw
            } else {
                aot_module.run()?
            };
            let result = nulang::vm::Value::from_raw(result_raw);
            let result_str = result.to_string_repr();
            if !result_str.is_empty() && result_str != "unit" && result_str != "()" {
                println!("{}", result_str);
            }
            Ok(())
        }
        "bytecode" => {
            // Bytecode backend (default).
            let m = compile_with_new_pipeline(&ast, "main", &type_checker)?;
            let constants = m.constants.clone();
            if verbose {
                println!("=== Bytecode (HIR/MIR pipeline) ===");
                println!("{}", disassemble(&m));
            }
            let has_actors = ast.decls.iter().any(|d| {
                matches!(
                    d,
                    nulang::ast::Decl::Actor { .. }
                        | nulang::ast::Decl::StateMachine { .. }
                        | nulang::ast::Decl::Workflow { .. }
                )
            });
            // Durable/event-sourced entities need a persistent store so
            // state survives restarts. Resolution order: `--store` flag,
            // `NULANG_STORE_PATH` env var, default `.nulang/store/`.
            // Programs without durable declarations keep the in-memory
            // store (as do `nula test` and the standalone VM paths).
            let has_durable = ast.decls.iter().any(|d| {
                matches!(
                    d,
                    nulang::ast::Decl::Actor {
                        persistent: true,
                        ..
                    }
                ) || matches!(
                    d,
                    nulang::ast::Decl::Actor { state_fields, .. }
                    if state_fields.iter().any(|(_, model, _, _)| matches!(
                        model,
                        nulang::ast::StateModel::Durable | nulang::ast::StateModel::EventSourced
                    ))
                )
            });
            let store_dir = if has_actors && has_durable {
                Some(
                    store_path
                        .map(|s| s.to_string())
                        .or_else(|| std::env::var("NULANG_STORE_PATH").ok())
                        .unwrap_or_else(|| ".nulang/store".to_string()),
                )
            } else {
                None
            };
            let value = if has_actors {
                let (value, runtime) = run_with_runtime(m, metrics_port, store_dir.as_deref())?;
                // Surface workflow step failures: a failed step used to be
                // silent (exit 0, no diagnostic) — SPEC2 §10 known-issue #5.
                let failures = runtime.borrow().workflow_failures();
                if !failures.is_empty() {
                    for (step_name, error) in &failures {
                        eprintln!("workflow step '{}' failed: {}", step_name, error);
                    }
                    std::process::exit(1);
                }
                if verbose {
                    let rt = runtime.borrow();
                    let snap = rt.metrics_snapshot();
                    match serde_json::to_string(&snap) {
                        Ok(json) => eprintln!("[metrics] {}", json),
                        Err(_) => eprintln!("[metrics] <serialization error>"),
                    }
                    eprintln!("{}", rt.render_topology());
                }
                value
            } else {
                let mut vm = VM::new();
                vm.load_module(m);
                vm.run()?
            };
            let result_str = if value.is_string() || value.is_ptr() {
                nulang::vm::resolve_value_string(&constants, value)
            } else {
                value.to_string_repr()
            };
            if !result_str.is_empty() && result_str != "unit" && result_str != "()" {
                println!("{}", result_str);
            }
            Ok(())
        }
        "core-vm" => {
            // Core VM backend: compile to bytecode, then run through minimal interpreter.
            let m = compile_with_new_pipeline(&ast, "main", &type_checker)?;
            if verbose {
                println!("=== Core VM (Stage 3) ===");
                println!("{}", disassemble(&m));
            }
            let mut vm = nulang::core_vm::CoreVM::new();
            let module_idx =
                vm.load_module_from_code(&m)
                    .map_err(|e| nulang::types::NuError::VMError {
                        msg: e,
                        span: Span::default(),
                    })?;
            let entry = m.entry_point.unwrap_or(0);
            let value = vm
                .run(module_idx, entry)
                .map_err(|e| nulang::types::NuError::VMError {
                    msg: e,
                    span: Span::default(),
                })?;
            let result_str = if let Some(s) = vm.resolve_display_string(value) {
                s
            } else {
                nulang::vm::Value::from_raw(value).to_string_repr()
            };
            if !result_str.is_empty() && result_str != "unit" && result_str != "()" {
                println!("{}", result_str);
            }
            Ok(())
        }
        _ => Err(nulang::types::NuError::VMError {
            msg: format!(
                "unknown backend '{}' (expected bytecode | native | core-vm{})",
                backend,
                if cfg!(feature = "wasm-backend") {
                    " | wasm | wasm-run | wasm-aot"
                } else {
                    ""
                }
            ),
            span: Span::default(),
        }),
    }
}

/// Swap a runtime's in-memory persistence store for a file-backed
/// [`JsonFileStore`](nulang::runtime::JsonFileStore) rooted at `dir`.
/// Used by `nulang run` when the program declares durable/event-sourced
/// entities so their state survives process restarts.
fn install_file_store(runtime: &mut nulang::runtime::Runtime, dir: &str) -> NuResult<()> {
    let store = nulang::runtime::JsonFileStore::new(dir).map_err(|e| NuError::RuntimeError {
        msg: format!("failed to open durable store at '{}': {}", dir, e),
        span: Span::default(),
    })?;
    runtime.persistence = Box::new(store);
    eprintln!("[durable] persistent store: {}", dir);
    Ok(())
}

/// Execute a module that declares actors against a real `Runtime`.
///
/// The top-level code runs on a VM with runtime-backed callbacks (so
/// `spawn` creates real actors and `send` enqueues real messages — the
/// same wiring the integration tests use), then the scheduler runs until
/// the run queue drains. Returns the top-level value and the runtime so
/// tests can inspect post-scheduling state.
fn run_with_runtime(
    m: nulang::bytecode::CodeModule,
    metrics_port: Option<u16>,
    store_dir: Option<&str>,
) -> NuResult<(
    nulang::vm::Value,
    std::rc::Rc<std::cell::RefCell<nulang::runtime::Runtime>>,
)> {
    // Multi-shard mode: controlled by NULANG_SHARDS env var (default 1).
    let num_shards: usize = std::env::var("NULANG_SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    if num_shards > 1 {
        // Create sharded runtimes upfront. Shard 0 serves the top-level
        // VM (all initial `spawn` calls land here); other shards run their
        // own schedulers in background threads.
        let mut shards = nulang::runtime::Runtime::new_sharded(num_shards);
        if let Some(dir) = store_dir {
            for shard in &mut shards {
                install_file_store(shard, dir)?;
            }
        }
        for shard in &mut shards {
            shard.register_module_grains(&m);
        }
        let remaining = shards.split_off(1);
        let mut shard_0 = shards.pop().unwrap();
        shard_0.register_module_grains(&m);

        let runtime = std::rc::Rc::new(std::cell::RefCell::new(shard_0));
        let mut vm = VM::new();
        vm.load_module(m);
        vm.set_actor_callbacks(Box::new(nulang::runtime::RuntimeVmCallbacks::new(
            runtime.clone(),
        )));
        let value = vm.run()?;
        // Drop the VM (and its callback box, which holds an Rc clone) so
        // we can unwrap the Rc below.
        drop(vm);

        let mut shard_0 = std::rc::Rc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("Rc has unexpected live clones"))
            .into_inner();
        if let Some(port) = metrics_port {
            let _ = shard_0.enable_metrics_server(port);
        }

        // Spawn worker threads for shards 1..N, run shard 0 on the main
        // thread so the return value captures its post-scheduler state.
        // When NULANG_PIN_CORES is set, bind each shard thread to its own
        // logical CPU to realize the thread-per-core model (shard i -> CPU i).
        let pin = nulang::runtime::core_pinning_enabled();
        if pin {
            let _ = nulang::runtime::pin_current_thread_to_cpu(0); // shard 0
        }
        std::thread::scope(|s| {
            for (idx, mut rt) in remaining.into_iter().enumerate() {
                let shard_idx = idx + 1; // shards[1..] => shard index 1..N
                s.spawn(move || {
                    if pin {
                        let _ = nulang::runtime::pin_current_thread_to_cpu(shard_idx);
                    }
                    rt.run_scheduler();
                });
            }
            shard_0.run_scheduler();
        });
        shard_0.publish_metrics();
        Ok((value, std::rc::Rc::new(std::cell::RefCell::new(shard_0))))
    } else {
        let runtime = std::rc::Rc::new(std::cell::RefCell::new(nulang::runtime::Runtime::new()));
        if let Some(dir) = store_dir {
            install_file_store(&mut runtime.borrow_mut(), dir)?;
        }
        runtime.borrow_mut().register_module_grains(&m);
        let mut vm = VM::new();
        vm.load_module(m);
        vm.set_actor_callbacks(Box::new(nulang::runtime::RuntimeVmCallbacks::new(
            runtime.clone(),
        )));
        if let Some(port) = metrics_port {
            let _ = runtime.borrow_mut().enable_metrics_server(port);
        }
        let value = vm.run()?;
        runtime.borrow_mut().run_scheduler();
        runtime.borrow().publish_metrics();
        Ok((value, runtime))
    }
}

fn check_source(
    source: &str,
    file_path: Option<&str>,
    verbose: bool,
    _all_errors: bool,
    with_capabilities: &[String],
    deny_warnings: bool,
) -> NuResult<()> {
    let (_ast, _tc) = run_frontend(source, file_path, verbose, with_capabilities, deny_warnings)?;

    if verbose {
        println!("Effect check passed.");
        println!("Capability analysis passed.");
    }

    Ok(())
}

/// Run the full frontend in multi-error mode and return every collected
/// per-declaration type error (empty when the module type-checks).
///
/// Drives `--check --all-errors`: the typechecker is configured with
/// `collect_errors = true` so `check_module` continues past failed
/// declarations instead of aborting at the first.
fn collect_all_frontend_errors(source: &str, file_path: Option<&str>) -> Vec<NuError> {
    use nulang::effect_checker::flatten_decls;
    // Lex + parse (fail fast; we need a parseable module to collect type errors).
    let (mut ast, _base_dir) = match parse_frontend(source, file_path) {
        Ok(pair) => pair,
        Err(e) => return vec![e],
    };
    if let Err(e) = nulang::resolver::resolve_imports(
        &mut ast,
        std::path::Path::new(file_path.unwrap_or(".")),
        &mut std::collections::HashSet::new(),
    ) {
        return vec![e];
    }
    let mut tc = TypeChecker::new();
    tc.collect_errors = true;
    let _ = tc.check_module(&ast);
    let mut errs = std::mem::take(&mut tc.collected_errors);
    if errs.is_empty() {
        return errs;
    }
    // Run the effect checker too so its accumulated diagnostics surface as errors.
    let mut ec = EffectChecker::new();
    if ec.check_module(&ast.decls).is_err() {
        for d in &ec.diagnostics {
            errs.push(NuError::effect_error(d.clone(), Span::default()));
        }
    }
    let _ = flatten_decls; // ensure import is used
    errs
}

/// Parse (lexer + parser) without the prelude/import machinery, returning the
/// AST and the base directory for import resolution.
fn parse_frontend(
    source: &str,
    file_path: Option<&str>,
) -> NuResult<(nulang::ast::AstModule, std::path::PathBuf)> {
    let mut lexer = Lexer::new(source);
    nulang::types::set_source_map_with_file(source, file_path);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module()?;
    let base_dir = std::path::Path::new(file_path.unwrap_or("."))
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    Ok((ast, base_dir))
}

fn compile_with_new_pipeline(
    ast: &nulang::ast::AstModule,
    name: &str,
    type_checker: &nulang::typechecker::TypeChecker,
) -> NuResult<nulang::bytecode::CodeModule> {
    // Anything this pipeline can't yet lower faithfully (see hir_lower.rs
    // and mir_lower.rs module docs) returns an honest NotYetImplemented
    // error, which the caller turns into a loud fallback to the stable
    // compiler.
    let hir = nulang::hir_lower::lower_module(ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir)?;
    nulang::mir_codegen::compile_mir(&mut mir, name)
}
/// Compile a source string to a `.nbc` artifact and write it to `out_path`.
///
/// The BLAKE3 hash of the source is recorded in the artifact header so a later
/// `--verify` run can confirm the artifact came from this exact source
/// (supply-chain integrity). Does not execute the module.
fn compile_source_to_nbc(
    source: &str,
    out_path: &str,
    rewrite_signals: Option<&str>,
    with_capabilities: &[String],
    deny_warnings: bool,
) -> NuResult<()> {
    let (mut ast, type_checker) =
        run_frontend(source, None, false, with_capabilities, deny_warnings)?;

    // Optional web-framework pass: rewrite HTML for signals/actions and emit the
    // generic client-side micro-runtime. This runs after effect checking so
    // action placements are known.
    if let Some(client_js_path) = rewrite_signals {
        let mut effect_checker = EffectChecker::new();
        effect_checker.check_module(&ast.decls)?;
        for msg in &effect_checker.diagnostics {
            eprintln!("{}", msg);
        }
        nulang::web::reactivity::rewrite_module(&mut ast, Some(&effect_checker));
        let client_js = nulang::web::reactivity::generate_client_runtime();
        std::fs::write(client_js_path, client_js).map_err(|e| nulang::types::NuError::VMError {
            msg: format!("failed to write {}: {}", client_js_path, e),
            span: Span::default(),
        })?;
    }
    let m = compile_with_new_pipeline(&ast, "main", &type_checker)?;
    let source_hash = blake3::hash(source.as_bytes());
    let bytes =
        m.to_nbc(Some(*source_hash.as_bytes()))
            .map_err(|e| nulang::types::NuError::VMError {
                msg: e.to_string(),
                span: Span::default(),
            })?;
    std::fs::write(out_path, &bytes).map_err(|e| nulang::types::NuError::VMError {
        msg: format!("failed to write {out_path}: {e}"),
        span: Span::default(),
    })?;
    println!(
        "Wrote {out_path} ({} bytes, .nbc format v{}, language v{})",
        bytes.len(),
        nulang::format::constants::BYTECODE_VERSION,
        nulang::format::constants::LANGUAGE_VERSION,
    );
    Ok(())
}

/// Load and run a `.nbc` artifact directly, optionally verifying its recorded
/// source hash against a source file. This is the durable-distribution path:
/// no compiler invocation, no source parse — just `from_nbc` + `VM::run`.
fn run_nbc_file(path: &str, verify_source: Option<&str>) -> NuResult<()> {
    let bytes = std::fs::read(path).map_err(|e| nulang::types::NuError::VMError {
        msg: format!("cannot read .nbc file '{path}': {e}"),
        span: Span::default(),
    })?;
    let artifact = nulang::bytecode::CodeModule::from_nbc(&bytes).map_err(|e| {
        nulang::types::NuError::VMError {
            msg: e.to_string(),
            span: Span::default(),
        }
    })?;

    if let Some(src_path) = verify_source {
        let source =
            std::fs::read_to_string(src_path).map_err(|e| nulang::types::NuError::VMError {
                msg: format!("cannot read source '{src_path}': {e}"),
                span: Span::default(),
            })?;
        let computed = blake3::hash(source.as_bytes());
        match artifact.source_hash {
            Some(h) if h == *computed.as_bytes() => { /* verified */ }
            Some(h) => {
                return Err(nulang::types::NuError::VMError {
                    msg: format!(
                        "source hash mismatch: artifact recorded {} but source {src_path} hashes to {}",
                        hex::encode(h),
                        hex::encode(computed.as_bytes()),
                    ),
                    span: Span::default(),
                });
            }
            None => {
                return Err(nulang::types::NuError::VMError {
                    msg: "artifact carries no source hash; cannot verify".into(),
                    span: Span::default(),
                });
            }
        }
    }

    let mut vm = VM::new();
    vm.load_module(artifact.module);
    let value = vm.run()?;
    let result_str = value.to_string_repr();
    if !result_str.is_empty() && result_str != "unit" && result_str != "()" {
        println!("{}", result_str);
    }
    Ok(())
}

fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Var(v) => format!("'t{}", v.0),
        Type::Primitive(p) => match p {
            nulang::types::PrimitiveType::Int => "Int".to_string(),
            nulang::types::PrimitiveType::Float => "Float".to_string(),
            nulang::types::PrimitiveType::Bool => "Bool".to_string(),
            nulang::types::PrimitiveType::String => "String".to_string(),
            nulang::types::PrimitiveType::Unit => "Unit".to_string(),
            nulang::types::PrimitiveType::Nil => "Nil".to_string(),
            nulang::types::PrimitiveType::Never => "Never".to_string(),
            nulang::types::PrimitiveType::Address => "Address".to_string(),
        },
        Type::Tuple(ts) => format!(
            "({})",
            ts.iter().map(type_to_string).collect::<Vec<_>>().join(", ")
        ),
        Type::Record(fs) => format!(
            "{{ {} }}",
            fs.iter()
                .map(|(n, t)| format!("{}: {}", n, type_to_string(t)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Variant(vs) => vs
            .iter()
            .map(|(n, t)| match t {
                Some(t) => format!("{} {}", n, type_to_string(t)),
                None => n.clone(),
            })
            .collect::<Vec<_>>()
            .join(" | "),
        Type::Array(t) => format!("[{}]", type_to_string(t)),
        Type::Function { param, ret, .. } => {
            format!("{} -> {}", type_to_string(param), type_to_string(ret))
        }
        Type::Actor { state, behavior } => format!(
            "Actor[{}, {}]",
            type_to_string(state),
            type_to_string(behavior)
        ),
        Type::App { constructor, args } => format!(
            "{}[{}]",
            type_to_string(constructor),
            args.iter()
                .map(type_to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Reference { cap, inner } => format!("&{:?} {}", cap, type_to_string(inner)),
        Type::Scheme { vars, body } => format!(
            "forall {}. {}",
            vars.iter()
                .map(|v| format!("'t{}", v.0))
                .collect::<Vec<_>>()
                .join(", "),
            type_to_string(body)
        ),
        Type::Nominal { name, .. } => name.clone(),
        Type::Skolem(id) => format!("'sk{}", id),
    }
}

fn disassemble(module: &nulang::bytecode::CodeModule) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    if !module.constants.is_empty() {
        writeln!(output, "Constants:").unwrap();
        for (i, c) in module.constants.iter().enumerate() {
            writeln!(output, "  {}: {:?}", i, c).unwrap();
        }
        writeln!(output).unwrap();
    }
    writeln!(output, "Instructions:").unwrap();
    for (i, instr) in module.instructions.iter().enumerate() {
        let op_name = format!("{:?}", instr.opcode);
        let comment = match instr.opcode {
            nulang::bytecode::OpCode::ConstU => module
                .constants
                .get(instr.imm16() as usize)
                .map(|c| format!("; load {:?}", c)),
            nulang::bytecode::OpCode::Call => Some(format!("; call R{}", instr.op1)),
            nulang::bytecode::OpCode::Closure => Some(format!("; closure @{}", instr.imm16())),
            nulang::bytecode::OpCode::Jmp
            | nulang::bytecode::OpCode::JmpT
            | nulang::bytecode::OpCode::JmpF => {
                Some(format!("; -> {}", i as i64 + instr.simm16() as i64))
            }
            _ => None,
        };
        match comment {
            Some(c) => writeln!(
                output,
                "  {:4}: {:12} {:3} {:3} {:3}    {}",
                i, op_name, instr.op1, instr.op2, instr.op3, c
            ),
            None => writeln!(
                output,
                "  {:4}: {:12} {:3} {:3} {:3}",
                i, op_name, instr.op1, instr.op2, instr.op3
            ),
        }
        .unwrap();
    }
    if !module.function_table.is_empty() {
        writeln!(output).unwrap();
        writeln!(output, "Function Table:").unwrap();
        for (i, offset) in module.function_table.iter().enumerate() {
            writeln!(output, "  {}: @{}", i, offset).unwrap();
        }
    }
    output
}

/// Simple Levenshtein distance for CLI flag suggestions.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An actor program run through the CLI path must create real actors
    /// and deliver sent messages: with the bare standalone VM the stub
    /// spawn/send callbacks would leave the counter at 0.
    #[test]
    fn test_run_source_actor_program_schedules_and_delivers() {
        let source = r#"
            actor Counter {
                state count: Int = 0
                behavior inc() { self.count = self.count + 1 }
            }
            let c = spawn Counter {} in {
                send c inc()
                send c inc()
                c
            }
        "#;
        let (ast, type_checker) = run_frontend(source, None, false, &[], false)
            .expect("frontend should accept the actor program");
        let module = compile_with_new_pipeline(&ast, "test", &type_checker)
            .expect("actor program should compile");
        let (_value, runtime) =
            run_with_runtime(module, None, None).expect("actor program should run");
        let rt = runtime.borrow();
        let actor = rt.actors.values().next().expect("one actor should exist");
        assert_eq!(
            actor.get_state_field("count").and_then(|v| v.as_int()),
            Some(2),
            "both inc messages must be delivered by run_scheduler"
        );
    }
}

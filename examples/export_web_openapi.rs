//! Use: cargo run --no-default-features --example export_web_openapi -- ./my-app/src
//! Prints validated OpenAPI 3.1 to stdout for JavaScript/Python tooling.
use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| {
        eprintln!("usage: export_web_openapi <source-root>");
        std::process::exit(2);
    });
    if args.next().is_some() || !Path::new(&root).exists() {
        eprintln!("expected exactly one existing source-root directory/file");
        std::process::exit(2);
    }
    let contracts =
        nulang::web::validation::compile_validated_contracts_from_tree(Path::new(&root))
            .unwrap_or_else(|diagnostics| {
                for message in diagnostics {
                    eprintln!("{message}");
                }
                std::process::exit(1);
            });
    let document = nulang::web::openapi::generate_openapi(&contracts, "Nulang Web API", "0.1.0");
    println!(
        "{}",
        serde_json::to_string_pretty(&document).expect("OpenAPI is serializable")
    );
}

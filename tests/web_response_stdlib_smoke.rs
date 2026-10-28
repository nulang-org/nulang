//! Compile the public response helpers through the normal CLI import pipeline,
//! rather than a standalone typechecker that does not link module functions.
use std::process::Command;

#[test]
fn public_response_helpers_typecheck_with_json_serializer_effects() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "nulang_web_response_stdlib_{}_{}.nula",
        std::process::id(),
        std::thread::current().name().unwrap_or("smoke")
    ));
    let program = r#"
import stdlib::web::types
import stdlib::web::response
import stdlib::json

@web_policy(body_max_bytes: "4096")
fn health() -> Json {
    json(JsonObject([("ok", JsonBool(true))]))
}

fn robots() -> Text {
    plaintext("User-agent: *\nDisallow: /private")
}

fn created_user() -> Created {
    created(JsonObject([("id", JsonNumber(42.0))]))
}

fn nothing() -> NoContent {
    no_content()
}

fn conflict() -> Problem {
    problem(409, "Conflict", "Already exists")
}

app "api" {
    route "GET" "/health" -> health
    route "GET" "/robots.txt" -> robots
    route "POST" "/users" -> created_user
    route "DELETE" "/users" -> nothing
    route "GET" "/conflict" -> conflict
}
"#;
    std::fs::write(&path, program).expect("write isolated Nulang smoke program");
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--check")
        .arg(&path)
        .env(
            "NULANG_STDLIB",
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/stdlib"),
        )
        .output()
        .expect("run Nulang CLI typecheck");
    let _ = std::fs::remove_file(&path);
    assert!(
        output.status.success(),
        "public response helper failed typecheck:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

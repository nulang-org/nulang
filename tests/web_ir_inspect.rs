//! Black-box contracts for the read-only Web deployment-IR inspector.
//! The inspector must never execute handlers or rediscover route metadata.

use nulang::web::inspect::inspect_ir_json;
use serde_json::json;

fn manifest(routes: serde_json::Value, version: u32) -> String {
    json!({
        "version": version,
        "routes": routes,
        "signals": {},
        "capabilities": [],
        "budgets": {},
        "middleware": [],
        "cloud_config": []
    })
    .to_string()
}

#[test]
fn lists_routes_in_deterministic_path_then_method_order() {
    let source = manifest(
        json!([
            {"method":"POST","path":"/products","placement":"server","artifact":null,"handler":"create"},
            {"method":"GET","path":"/","placement":"static","artifact":"index.html","handler":"home","response_type":"Html"},
            {"method":"GET","path":"/products","placement":"server","artifact":null,"handler":"index"}
        ]),
        1,
    );
    let output = inspect_ir_json(&source, None).expect("valid IR");

    let lines: Vec<_> = output.lines().collect();
    assert_eq!(
        lines,
        [
            "METHOD\tPATH\tPLACEMENT\tHANDLER\tRESPONSE",
            "GET\t/\tstatic\thome\tHtml",
            "GET\t/products\tserver\tindex\t-",
            "POST\t/products\tserver\tcreate\t-",
        ]
    );
}

#[test]
fn inspects_exact_route_and_reports_effects_without_recompiling() {
    let source = manifest(
        json!([{
            "method": "POST",
            "path": "/products",
            "placement": "server",
            "artifact": null,
            "handler": "create",
            "response_type": "Json[Product]",
            "error_type": "CreateError",
            "effects": ["DB", "Request"]
        }]),
        1,
    );
    let output = inspect_ir_json(&source, Some(("POST", "/products"))).unwrap();

    assert!(output.contains("Route: POST /products"));
    assert!(output.contains("Placement: server"));
    assert!(output.contains("Handler: create"));
    assert!(output.contains("Response: Json[Product]"));
    assert!(output.contains("Error: CreateError"));
    assert!(output.contains("Effects: DB, Request"));
    assert!(output.contains("Artifact: -"));
}

#[test]
fn rejects_missing_and_duplicate_route_claims() {
    let source = manifest(
        json!([{"method":"GET","path":"/","placement":"static","artifact":"index.html"}]),
        1,
    );
    assert!(inspect_ir_json(&source, Some(("POST", "/")))
        .unwrap_err()
        .contains("not found"));

    let duplicate = manifest(
        json!([
            {"method":"GET","path":"/","placement":"static","artifact":"index.html"},
            {"method":"GET","path":"/","placement":"server","artifact":null}
        ]),
        1,
    );
    assert!(inspect_ir_json(&duplicate, None)
        .unwrap_err()
        .contains("duplicate route"));
}

#[test]
fn refuses_incompatible_or_incomplete_deployment_ir() {
    let unsupported = manifest(json!([]), 2);
    assert!(inspect_ir_json(&unsupported, None)
        .unwrap_err()
        .contains("unsupported deployment IR version"));

    assert!(inspect_ir_json("{\"version\":1}", None).is_err());
    assert!(inspect_ir_json("not JSON", None).is_err());
}

#[test]
fn refuses_control_characters_in_displayed_route_fields() {
    let source = manifest(
        json!([{
            "method": "GET",
            "path": "/unsafe\tfield",
            "placement": "static",
            "artifact": "index.html"
        }]),
        1,
    );
    assert!(inspect_ir_json(&source, None)
        .unwrap_err()
        .contains("control character"));
}

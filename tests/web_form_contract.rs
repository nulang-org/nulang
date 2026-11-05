use nulang::bytecode::Constant;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::forms::{FormContract, FormRule};
use nulang::web::request_bindings::parse_urlencoded_multi;

fn lead_form() -> FormContract {
    let source = r#"
fn lead(
    company: String from form("company"),
    seats: Int from form("seats"),
    annual: Float from form("annual"),
    subscribed: Bool from form("subscribed")
) -> String { "ok" }

fn web_main() {
    perform Web.route("POST", "/leads", lead)
}
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    let module = Parser::new(tokens).parse_module().unwrap();
    let compiled = compile_module_contracts(&module);
    assert!(
        compiled.diagnostics.is_empty(),
        "{:?}",
        compiled.diagnostics
    );
    FormContract::from_route(&compiled.routes[0])
        .unwrap()
        .with_rules(
            "company",
            vec![FormRule::MinLength(2), FormRule::MaxLength(80)],
        )
        .unwrap()
        .with_rules("seats", vec![FormRule::MinInt(1), FormRule::MaxInt(20)])
        .unwrap()
        .with_rules("annual", vec![FormRule::MinFloat(0.0)])
        .unwrap()
}

#[test]
fn form_schema_comes_from_compiler_owned_route_bindings_and_decodes_typed_values() {
    let contract = lead_form();
    assert_eq!(contract.method, "POST");
    assert_eq!(contract.path, "/leads");
    assert_eq!(contract.fields.len(), 4);
    let fields = parse_urlencoded_multi(
        b"company=Acme+%26+Co&seats=5&annual=100.25&subscribed=false&csrf=ignored",
    );
    let output = contract.validate(&fields).unwrap();
    assert_eq!(
        output.values["company"],
        Constant::String("Acme & Co".into())
    );
    assert_eq!(output.values["seats"], Constant::Int(5));
    assert_eq!(output.values["annual"], Constant::Float(100.25));
    assert_eq!(output.values["subscribed"], Constant::Bool(false));
    assert_eq!(output.values.len(), 4);
}

#[test]
fn form_validation_aggregates_field_errors_with_machine_readable_422_problem() {
    let fields = parse_urlencoded_multi(b"company=x&seats=0&annual=NaN");
    let err = lead_form().validate(&fields).unwrap_err();
    let summary: Vec<_> = err
        .issues
        .iter()
        .map(|error| (error.field.as_str(), error.code))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("company", "too_short"),
            ("seats", "too_small"),
            ("annual", "invalid_type"),
            ("subscribed", "required"),
        ]
    );
    let problem = err.problem_details();
    assert_eq!(problem["status"], 422);
    assert_eq!(problem["errors"][0]["field"], "company");
    assert_eq!(problem["errors"][1]["code"], "too_small");
    assert!(!problem.to_string().contains("NaN"));
}

#[test]
fn form_validation_rejects_repeated_values_including_encoded_aliases() {
    let fields =
        parse_urlencoded_multi(b"company=Good&%63ompany=Other&seats=5&annual=10&subscribed=true");
    let errors = lead_form().validate(&fields).unwrap_err();
    assert_eq!(errors.issues.len(), 1);
    assert_eq!(errors.issues[0].field, "company");
    assert_eq!(errors.issues[0].code, "multiple");
}

#[test]
fn form_rules_are_checked_against_compiler_field_types_and_bounds() {
    assert!(lead_form()
        .with_rules("missing", vec![FormRule::MinLength(1)])
        .is_err());
    assert!(lead_form()
        .with_rules("seats", vec![FormRule::MinLength(1)])
        .is_err());
    assert!(lead_form()
        .with_rules(
            "company",
            vec![FormRule::MinLength(9), FormRule::MaxLength(2)]
        )
        .is_err());
    assert!(lead_form()
        .with_rules("annual", vec![FormRule::MinFloat(f64::NAN)])
        .is_err());
    assert!(lead_form()
        .with_rules("seats", vec![FormRule::MinInt(2), FormRule::MinInt(3)])
        .is_err());
}

#[test]
fn duplicate_source_fields_are_rejected_when_building_a_form_contract() {
    let source = r#"
fn lead(a: String from form("tag"), b: String from form("tag")) -> String { "ok" }
fn web_main() { perform Web.route("POST", "/leads", lead) }
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    let module = Parser::new(tokens).parse_module().unwrap();
    let compiled = compile_module_contracts(&module);
    let err = FormContract::from_route(&compiled.routes[0]).unwrap_err();
    assert!(err
        .iter()
        .any(|message| message.contains("duplicate form field")));
}

#[test]
fn generated_form_fields_are_accessible_and_escape_untrusted_content() {
    let input =
        parse_urlencoded_multi(b"company=%3Cscript%3E%26%22&seats=0&annual=10&subscribed=true");
    let errors = lead_form().validate(&input).unwrap_err();
    let html = lead_form().render_fields(&input, Some(&errors));
    assert!(html.contains("<label for=\"nulang-field-0\">company</label>"));
    assert!(html.contains("aria-invalid=\"true\""));
    assert!(html.contains("aria-describedby=\"nulang-field-1-error\""));
    assert!(html.contains("role=\"alert\""));
    assert!(html.contains("&lt;script&gt;&amp;&quot;"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("name=\"subscribed\""));
    assert!(html.contains("<select"));
    assert!(html.contains("minlength=\"2\""));
    assert!(html.contains("max=\"20\""));
}

#[test]
fn empty_required_string_is_rejected_even_without_minimum_length() {
    let submitted = parse_urlencoded_multi(b"company=&seats=5&annual=3&subscribed=false");
    let errors = lead_form().validate(&submitted).unwrap_err();
    assert_eq!(errors.issues.len(), 1);
    assert_eq!(errors.issues[0].field, "company");
    assert_eq!(errors.issues[0].code, "required");
}

#[test]
fn form_validation_problems_can_be_returned_through_existing_http_adapter() {
    use nulang::web::http_problem::form_validation_problem_response;
    let submitted = parse_urlencoded_multi(b"company=ok&seats=0&annual=3&subscribed=true");
    let errors = lead_form().validate(&submitted).unwrap_err();
    let response = form_validation_problem_response(&errors);
    assert_eq!(response.status, 422);
    assert_eq!(
        response.headers,
        vec![(
            "Content-Type".to_string(),
            "application/problem+json".to_string()
        )]
    );
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["errors"][0]["field"], "seats");
    assert_eq!(body["errors"][0]["code"], "too_small");
}

#[test]
fn maximum_lengths_and_integer_bounds_do_not_truncate_or_round() {
    let fields = parse_urlencoded_multi(
        b"company=abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabc&seats=21&annual=10&subscribed=true"
    );
    let errors = lead_form().validate(&fields).unwrap_err();
    assert_eq!(errors.issues.len(), 2);
    assert_eq!(errors.issues[0].code, "too_long");
    assert_eq!(errors.issues[1].code, "too_large");

    let fields =
        parse_urlencoded_multi(b"company=Acme&seats=9223372036854775807&annual=10&subscribed=true");
    let errors = lead_form().validate(&fields).unwrap_err();
    assert_eq!(errors.issues[0].code, "too_large");
}

#[test]
fn browser_only_validation_cannot_bypass_server_type_rules() {
    let submitted =
        parse_urlencoded_multi(b"company=Acme&seats=not-a-number&annual=0&subscribed=on");
    let errors = lead_form().validate(&submitted).unwrap_err();
    assert_eq!(errors.issues.len(), 2);
    assert_eq!(errors.issues[0].code, "invalid_type");
    assert_eq!(errors.issues[1].code, "invalid_type");
}

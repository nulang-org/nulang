use nulang::bytecode::Constant;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::forms::{FormContract, FormFieldShape};
use nulang::web::request_bindings::parse_urlencoded_multi;

fn form() -> FormContract {
    let source = r#"
fn endpoint(name: String from form("name"), ages: Int from form("ages"), note: String from form("note")) -> String { name }
fn web_main() { perform Web.route("POST", "/signup", endpoint) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let c = compile_module_contracts(&ast);
    assert!(c.diagnostics.is_empty());
    FormContract::from_route(&c.routes[0]).unwrap()
}

#[test]
fn optional_scalar_is_absent_without_null_string_coercion() {
    let schema = form().with_shape("note", FormFieldShape::Optional).unwrap();
    let values = parse_urlencoded_multi(b"name=Alice&ages=20");
    let decoded = schema.validate(&values).unwrap();
    assert!(!decoded.values.contains_key("note"));
    assert!(decoded.repeated.is_empty());
    let html = schema.render_fields(&values, None);
    assert!(!html.contains("name=\"note\" required"));
    assert!(html.contains("name=\"name\" required"));
    let submitted = parse_urlencoded_multi(b"name=Alice&ages=20&note=hello");
    assert_eq!(
        schema.validate(&submitted).unwrap().values["note"],
        Constant::String("hello".into())
    );
}

#[test]
fn repeated_typed_int_values_are_collected_without_last_wins_coercion() {
    let schema = form()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 1,
                max_items: 3,
            },
        )
        .unwrap()
        .with_shape("note", FormFieldShape::Optional)
        .unwrap();
    let values = parse_urlencoded_multi(b"name=Alice&ages=20&%61ges=21");
    let decoded = schema.validate(&values).unwrap();
    assert_eq!(
        decoded.repeated["ages"],
        vec![Constant::Int(20), Constant::Int(21)]
    );
    assert!(!decoded.values.contains_key("ages"));
    let html = schema.render_fields(&values, None);
    assert!(html.contains("name=\"ages\""));
    assert!(html.contains("value=\"20\""));
    assert!(html.contains("value=\"21\""));
    assert!(html.contains("fieldset"));
    assert!(html.contains("for=\"nulang-field-1-item-0\""));
}

#[test]
fn repeated_items_have_bounds_and_field_error_codes() {
    let schema = form()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 1,
                max_items: 2,
            },
        )
        .unwrap()
        .with_shape("note", FormFieldShape::Optional)
        .unwrap();
    let err = schema
        .validate(&parse_urlencoded_multi(b"name=Alice"))
        .unwrap_err();
    assert_eq!(err.issues[0].code, "too_few");
    let err = schema
        .validate(&parse_urlencoded_multi(b"name=Alice&ages=1&ages=2&ages=3"))
        .unwrap_err();
    assert_eq!(err.issues[0].code, "too_many");
    let err = schema
        .validate(&parse_urlencoded_multi(b"name=Alice&ages=1&ages=oops"))
        .unwrap_err();
    assert_eq!(err.issues[0].code, "invalid_type");
    assert!(!err.problem_details().to_string().contains("oops"));
}

#[test]
fn invalid_collection_shape_and_mode_mutation_fail_closed() {
    assert!(form()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 3,
                max_items: 2
            }
        )
        .is_err());
    assert!(form()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 0,
                max_items: 0
            }
        )
        .is_err());
    assert!(form()
        .with_shape("unknown", FormFieldShape::Optional)
        .is_err());
    assert!(form()
        .with_shape("ages", FormFieldShape::Optional)
        .unwrap()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 0,
                max_items: 5
            }
        )
        .is_err());
}

#[test]
fn optional_and_repeated_decode_reject_ambiguous_or_hostile_inputs() {
    let optional = form().with_shape("note", FormFieldShape::Optional).unwrap();
    let duplicate = parse_urlencoded_multi(b"name=Alice&ages=1&note=a&%6eote=b");
    let err = optional.validate(&duplicate).unwrap_err();
    assert_eq!(err.issues[0].field, "note");
    assert_eq!(err.issues[0].code, "multiple");

    let repeated = form()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 0,
                max_items: 2,
            },
        )
        .unwrap();
    let input =
        parse_urlencoded_multi(b"name=Alice&note=hi&ages=%3Cimg+src%3Dx+onerror%3D1%3E&ages=2");
    let err = repeated.validate(&input).unwrap_err();
    assert_eq!(err.issues[0].code, "invalid_type");
    let html = repeated.render_fields(&input, Some(&err));
    assert!(!html.contains("<img"));
    assert!(html.contains("&lt;img"));
    assert!(html.contains("aria-describedby=\"nulang-field-1-error\""));
}

#[test]
fn optional_empty_input_is_absent_not_an_implicit_empty_string() {
    let optional = form().with_shape("note", FormFieldShape::Optional).unwrap();
    let values = parse_urlencoded_multi(b"name=Alice&ages=1&note=");
    let output = optional.validate(&values).unwrap();
    assert!(!output.values.contains_key("note"));
}

#[test]
fn repeated_empty_collection_with_zero_minimum_is_preserved() {
    let repeated = form()
        .with_shape(
            "ages",
            FormFieldShape::Repeated {
                min_items: 0,
                max_items: 3,
            },
        )
        .unwrap();
    let output = repeated
        .validate(&parse_urlencoded_multi(b"name=Alice&note=hi"))
        .unwrap();
    assert_eq!(output.repeated["ages"], Vec::<Constant>::new());
}

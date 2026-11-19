//! Black-box conformance tests for the experimental pure relational Query IR.
use std::collections::BTreeMap;

use nulang::query::{
    execute, explain, optimize, Catalog, Column, DataType, Expr, Plan, Row, Scalar, Table,
};

fn row(values: &[(&str, Scalar)]) -> Row {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), value.clone()))
        .collect()
}

fn catalog() -> Catalog {
    let users = Table::new(
        vec![
            Column::new("id", DataType::Int),
            Column::new("age", DataType::Int),
            Column::new("name", DataType::Text),
        ],
        vec![
            row(&[
                ("id", Scalar::Int(1)),
                ("age", Scalar::Int(20)),
                ("name", Scalar::Text("Ada".into())),
            ]),
            row(&[
                ("id", Scalar::Int(2)),
                ("age", Scalar::Int(17)),
                ("name", Scalar::Text("Lin".into())),
            ]),
            row(&[
                ("id", Scalar::Int(3)),
                ("age", Scalar::Int(30)),
                ("name", Scalar::Text("Grace".into())),
            ]),
        ],
    )
    .unwrap();
    let mut catalog = Catalog::new();
    catalog.insert("users", users).unwrap();
    catalog
}

#[test]
fn relational_query_filters_sorts_and_projects_without_sql_strings() {
    let query = Plan::scan("users")
        .filter(Expr::gt(Expr::field("age"), Expr::int(18)))
        .sort("age", true)
        .project(vec![("name", Expr::field("name"))]);
    assert_eq!(
        execute(&query, &catalog()).unwrap(),
        vec![
            row(&[("name", Scalar::Text("Grace".into()))]),
            row(&[("name", Scalar::Text("Ada".into()))]),
        ]
    );
}

#[test]
fn filter_pushes_through_sort_without_changing_results() {
    let query = Plan::scan("users")
        .sort("age", false)
        .filter(Expr::gt(Expr::field("age"), Expr::int(18)));
    let optimized = optimize(&query, &catalog()).unwrap();
    assert_eq!(
        execute(&query, &catalog()).unwrap(),
        execute(&optimized, &catalog()).unwrap()
    );
    assert_eq!(
        explain(&optimized),
        "Sort age ASC\n  Filter (age > 18)\n    Scan users"
    );
}

#[test]
fn limit_is_an_optimization_boundary() {
    // Filtering *after* taking the youngest user yields no rows.
    let query = Plan::scan("users")
        .sort("age", false)
        .limit(1)
        .filter(Expr::gt(Expr::field("age"), Expr::int(18)));
    let optimized = optimize(&query, &catalog()).unwrap();
    assert_eq!(execute(&query, &catalog()).unwrap(), Vec::<Row>::new());
    assert_eq!(
        execute(&query, &catalog()).unwrap(),
        execute(&optimized, &catalog()).unwrap()
    );
    assert!(explain(&optimized).starts_with("Filter (age > 18)\n  Limit 1"));
}

#[test]
fn validator_rejects_invalid_predicates_and_unknown_fields() {
    let wrong_type = Plan::scan("users").filter(Expr::gt(Expr::field("age"), Expr::text("18")));
    assert!(optimize(&wrong_type, &catalog()).is_err());
    assert!(execute(&wrong_type, &catalog()).is_err());

    let unknown = Plan::scan("users").filter(Expr::eq(Expr::field("password"), Expr::text("x")));
    assert!(optimize(&unknown, &catalog()).is_err());
}

#[test]
fn storage_rows_are_strictly_typed_and_missing_is_not_null() {
    let result = Table::new(
        vec![Column::new("age", DataType::Int)],
        vec![BTreeMap::new()],
    );
    assert!(result.is_err());

    let result = Table::new(
        vec![Column::new("age", DataType::Int)],
        vec![row(&[("age", Scalar::Text("20".into()))])],
    );
    assert!(result.is_err());
}

#[test]
fn local_queries_do_not_implicitly_cast_integer_and_text_values() {
    let query = Plan::scan("users").filter(Expr::eq(Expr::field("id"), Expr::text("1")));
    assert!(execute(&query, &catalog()).is_err());
}

#[test]
fn optimized_plans_have_readable_deterministic_explanations() {
    let query = Plan::scan("users")
        .filter(Expr::gt(Expr::field("age"), Expr::int(18)))
        .project(vec![("person", Expr::field("name"))])
        .limit(2);
    assert_eq!(
        explain(&query),
        "Limit 2\n  Project person=name\n    Filter (age > 18)\n      Scan users"
    );
}

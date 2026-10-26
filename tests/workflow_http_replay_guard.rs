//! HTTP is a network effect: unjournaled HTTP calls may not replay inside
//! durable workflow steps, compensations, or persistent actor behaviors.
use nulang::effect_checker::EffectChecker;
use nulang::lexer::Lexer;
use nulang::parser::Parser;

fn check_effects(src: &str) -> Result<(), String> {
    let tokens = Lexer::new(src).lex().map_err(|e| e.to_string())?;
    let module = Parser::new(tokens)
        .parse_module()
        .map_err(|e| e.to_string())?;
    EffectChecker::new()
        .check_module(&module.decls)
        .map_err(|e| e.to_string())
}

#[test]
fn rejects_http_get_in_durable_workflow_step() {
    let err =
        check_effects("workflow W { step fetch { perform Http.get(\"http://127.0.0.1:1/\") } }")
            .expect_err("HTTP GET inside replay must be denied");
    assert!(
        err.contains("workflow 'W' step 'fetch'")
            && err.contains("effect 'Http.get' is not allowed in durable replay surfaces"),
        "{err}"
    );
}

#[test]
fn rejects_http_post_via_helper_in_durable_step() {
    let err = check_effects(
        "fn push() { perform Http.post(\"https://example.invalid\", \"data\") }\n\
         workflow W { step dispatch { push() } }",
    )
    .expect_err("HTTP POST through helper inside replay must be denied");
    assert!(
        err.contains("workflow 'W' step 'dispatch'")
            && err.contains("effect 'Http.post' is not allowed in durable replay surfaces"),
        "{err}"
    );
}

#[test]
fn rejects_http_in_workflow_compensation() {
    let err = check_effects(
        "workflow W { step prepare { 1 } compensate { perform Http.post(\"https://example.invalid\", \"refund\") } }",
    ).expect_err("HTTP compensation without durable receipt must be denied");
    assert!(
        err.contains("workflow 'W' step 'prepare'")
            && err.contains("effect 'Http.post' is not allowed in durable replay surfaces"),
        "{err}"
    );
}

#[test]
fn rejects_http_in_persistent_actor_behavior() {
    let err = check_effects(
        "persistent actor A { state n: Int = 0\n behavior ping() { perform Http.get(\"https://example.invalid\") } }",
    ).expect_err("HTTP in persistent actor replay must be denied");
    assert!(
        err.contains("behavior 'ping' of persistent actor 'A'")
            && err.contains("effect 'Http.get' is not allowed in durable replay surfaces"),
        "{err}"
    );
}

#[test]
fn allows_ordinary_http_outside_replay_context() {
    check_effects("fn fetch() { perform Http.get(\"https://example.invalid\") }")
        .expect("non-durable HTTP stays permitted by replay guard");
}

#[test]
fn allows_journaled_timer_in_workflow() {
    check_effects("workflow W { step wait { perform Timer.sleep(\"receipt\", 1) } }")
        .expect("journaled timers remain allowed");
}

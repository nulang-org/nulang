use nulang::bytecode::ActorMeta;
use nulang::mir::Module;
use nulang::mir_codegen::compile_mir;

#[test]
fn codegen_rejects_conflicting_actor_roles_before_specialized_dispatch() {
    let mut meta = ActorMeta::new("conflicting");
    meta.is_agent = true;
    meta.is_workflow = true;

    let mut module = Module {
        name: "actor-role-conflict".to_string(),
        functions: Vec::new(),
        behaviors: Vec::new(),
        actor_metadata: vec![meta],
        compensation_of: Vec::new(),
        parallel_branches_of: Vec::new(),
        foreign_functions: Vec::new(),
    };

    let err = compile_mir(&mut module, "actor-role-conflict")
        .expect_err("conflicting actor metadata must fail closed");
    let message = err.to_string();

    assert!(
        message.contains("actor metadata has conflicting roles"),
        "unexpected error: {message}"
    );
    assert!(message.contains("workflow=true"), "{message}");
    assert!(message.contains("agent=true"), "{message}");
}

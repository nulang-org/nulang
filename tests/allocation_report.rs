//! Contract tests for read-only bytecode aggregate-allocation reporting.
//! The report must never claim that an arena allocation happened at runtime.
use nulang::allocation_report::{analyze_aggregate_allocations, AllocationDisposition};
use nulang::bytecode::{CodeModule, Instruction, OpCode};

fn sample_module() -> CodeModule {
    let mut module = CodeModule::new("allocation-fixture");
    // Not supported by the arena analysis, regardless of whether it escapes.
    module.emit(Instruction::new3(OpCode::RecCopy, 0, 0, 4)); // pc 0
    module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 1)); // pc 1
    module.emit(Instruction::new1(OpCode::Drop, 1)); // pc 2
    module.emit(Instruction::new2(OpCode::TupleMk, 0, 3)); // pc 3
    module.emit(Instruction::new1(OpCode::Drop, 3)); // pc 4
    module.emit(Instruction::new2(OpCode::RecMk, 0, 2)); // pc 5
    module.emit(Instruction::new1(OpCode::RetVal, 2)); // pc 6: escapes
    module
}

#[test]
fn classifies_every_supported_aggregate_site_in_instruction_order() {
    let module = sample_module();
    let before = module.instructions.clone();

    let report = analyze_aggregate_allocations(&module);

    assert_eq!(report.schema_version, 1);
    assert_eq!(report.module, "allocation-fixture");
    assert_eq!(report.total_sites, 4);
    assert_eq!(report.arena_eligible, 2);
    assert_eq!(report.not_proven, 1);
    assert_eq!(report.not_analyzed, 1);
    assert_eq!(report.sites.iter().map(|site| site.pc).collect::<Vec<_>>(), vec![0, 1, 3, 5]);
    assert_eq!(report.sites[0].disposition, AllocationDisposition::NotAnalyzed);
    assert_eq!(report.sites[1].disposition, AllocationDisposition::ArenaEligible);
    assert_eq!(report.sites[2].disposition, AllocationDisposition::ArenaEligible);
    assert_eq!(report.sites[3].disposition, AllocationDisposition::NotProven);
    assert_eq!(module.instructions, before, "analysis may not mutate bytecode");
}

#[test]
fn report_fails_closed_for_escaping_actor_messages() {
    let mut module = CodeModule::new("send-escapes");
    module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 1));
    module.emit(Instruction::new2(OpCode::Move, 1, 0));
    module.emit(Instruction::new3(OpCode::Send, 2, 0, 0));
    module.emit(Instruction::new0(OpCode::Halt));

    let report = analyze_aggregate_allocations(&module);

    assert_eq!(report.total_sites, 1);
    assert_eq!(report.arena_eligible, 0);
    assert_eq!(report.not_proven, 1);
    assert_eq!(report.sites[0].disposition, AllocationDisposition::NotProven);
}

#[test]
fn report_is_stable_json_even_when_module_has_no_allocations() {
    let mut module = CodeModule::new("empty");
    module.emit(Instruction::new0(OpCode::Halt));

    let report = analyze_aggregate_allocations(&module);
    let json = serde_json::to_value(&report).expect("report should serialize");

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["module"], "empty");
    assert_eq!(json["total_sites"], 0);
    assert_eq!(json["arena_eligible"], 0);
    assert_eq!(json["not_proven"], 0);
    assert_eq!(json["not_analyzed"], 0);
    assert_eq!(json["sites"], serde_json::json!([]));
}

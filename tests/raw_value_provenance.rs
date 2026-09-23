use nulang::value_layout::TAG_PTR;
use nulang::vm::Value;

#[test]
fn unchecked_raw_constructors_remain_explicitly_unsafe() {
    let vm_source = include_str!("../src/vm.rs");

    assert!(
        vm_source.contains("pub unsafe fn from_raw(raw: u64) -> Self"),
        "Value::from_raw must remain an explicit unsafe boundary"
    );
    assert!(
        vm_source.contains("pub unsafe fn from_bits(raw: u64) -> Self"),
        "Value::from_bits must remain an explicit unsafe boundary"
    );
    assert!(
        !vm_source.contains("pub fn from_raw(raw: u64) -> Self"),
        "safe arbitrary raw reconstruction must not return"
    );
    assert!(
        !vm_source.contains("pub fn from_bits(raw: u64) -> Self"),
        "safe arbitrary bit reconstruction must not return"
    );
}

#[test]
fn untrusted_raw_decoder_rejects_host_pointer_tags() {
    let forged_pointer = TAG_PTR | 0x1234;
    assert!(
        Value::try_from_untrusted_bits(forged_pointer).is_err(),
        "externally controlled TAG_PTR bits must fail closed"
    );

    let immediate = Value::int(42).as_raw();
    let decoded = Value::try_from_untrusted_bits(immediate)
        .expect("ordinary immediate values remain valid across untrusted boundaries");
    assert_eq!(decoded.as_int(), Some(42));
}

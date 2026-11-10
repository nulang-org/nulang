use nulang::value_layout::{TAG_ACTOR, TAG_CLOSURE, TAG_OBJECT, TAG_PTR, TAG_STRING};
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

#[test]
fn untrusted_raw_decoder_rejects_forged_host_handles() {
    for (kind, tag) in [
        ("actor", TAG_ACTOR),
        ("closure", TAG_CLOSURE),
        ("object", TAG_OBJECT),
    ] {
        let forged = tag | 0x1234;
        assert!(
            Value::try_from_untrusted_bits(forged).is_err(),
            "untrusted raw bits must not invent a live {kind} handle"
        );
    }
}

#[test]
fn untrusted_raw_decoder_keeps_interned_string_ids_for_wasm_compatibility() {
    let value = Value::try_from_untrusted_bits(TAG_STRING | 2)
        .expect("guest-relative interned string IDs are not host heap pointers");
    assert_eq!(value.as_raw(), TAG_STRING | 2);
}

#[test]
fn trusted_actor_values_still_use_explicit_runtime_constructors() {
    let actor = Value::actor_ref(123);
    assert_eq!(actor.as_actor_id(), Some(123));
}

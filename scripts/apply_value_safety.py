#!/usr/bin/env python3
"""Apply the one-shot Value raw-bit safety migration.

This script is intentionally strict: every structural replacement must match
exactly once, and it aborts rather than guessing when the source differs.
It is removed from the branch after the generated patch is committed.
"""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:80]!r}")
    path.write_text(text.replace(old, new, 1))


def wrap_value_calls(path: Path, method: str) -> int:
    """Wrap qualified `...Value::<method>(...)` calls in an unsafe block."""
    text = path.read_text()
    needle = f"Value::{method}("
    out: list[str] = []
    cursor = 0
    wrapped = 0

    while True:
        value_idx = text.find(needle, cursor)
        if value_idx < 0:
            out.append(text[cursor:])
            break

        # Include a complete Rust path such as `crate::vm::Value` or
        # `nulang::vm::Value`, rather than wrapping only the `Value` suffix.
        start = value_idx
        while start > 0 and (text[start - 1].isalnum() or text[start - 1] in "_:"):
            start -= 1

        prefix = text[max(0, start - 9):start]
        if prefix.endswith("unsafe { "):
            out.append(text[cursor:value_idx + len(needle)])
            cursor = value_idx + len(needle)
            continue

        open_paren = value_idx + len(needle) - 1
        depth = 0
        i = open_paren
        in_string = False
        escape = False
        while i < len(text):
            ch = text[i]
            if escape:
                escape = False
            elif ch == "\\" and in_string:
                escape = True
            elif ch == '"':
                in_string = not in_string
            elif not in_string:
                if ch == "(":
                    depth += 1
                elif ch == ")":
                    depth -= 1
                    if depth == 0:
                        end = i + 1
                        break
            i += 1
        else:
            raise SystemExit(f"{path}: unterminated Value::{method} call at byte {value_idx}")

        out.append(text[cursor:start])
        out.append("unsafe { ")
        out.append(text[start:end])
        out.append(" }")
        cursor = end
        wrapped += 1

    path.write_text("".join(out))
    return wrapped


vm = ROOT / "src/vm.rs"
replace_once(
    vm,
    """    /// Create a pointer value (for strings, lists, etc.).\n    pub fn ptr(p: *mut u8) -> Self {\n        Value {\n            raw: TAG_PTR | (p as u64 & PAYLOAD_MASK),\n        }\n    }\n""",
    """    /// Create a pointer value (for strings, lists, etc.).\n    ///\n    /// Panics instead of silently truncating addresses that do not fit the\n    /// legacy 48-bit pointer payload.\n    ///\n    /// # Safety\n    /// A non-null `p` must remain valid for every operation that may\n    /// dereference the returned `Value` (for example string resolution or\n    /// heap-object traversal). The pointer must refer to storage whose layout\n    /// matches the tag's consumer expectations.\n    pub unsafe fn ptr(p: *mut u8) -> Self {\n        let addr = p as u64;\n        assert!(\n            crate::value_layout::ptr_fits_payload(addr),\n            \"pointer address does not fit Nulang's 48-bit Value payload\"\n        );\n        Value { raw: TAG_PTR | addr }\n    }\n""",
)
replace_once(
    vm,
    """    /// Construct a Value from raw NaN-boxed bits.\n    ///\n    /// # Safety\n    /// The caller must ensure the bits form a valid tagged value.\n    pub fn from_raw(raw: u64) -> Self {\n        Value { raw }\n    }\n""",
    """    /// Construct a `Value` from raw tagged bits.\n    ///\n    /// # Safety\n    /// If `raw` has `TAG_PTR`, its payload must be a live pointer produced by\n    /// a Nulang heap and remain valid for every use of the returned value.\n    /// Other tagged payloads must satisfy their tag-specific invariants.\n    pub unsafe fn from_raw(raw: u64) -> Self {\n        Value { raw }\n    }\n""",
)
replace_once(
    vm,
    """    /// Construct a Value from raw NaN-boxed bits.\n    pub fn from_bits(raw: u64) -> Self {\n        Value { raw }\n    }\n""",
    """    /// Construct a `Value` from raw tagged bits.\n    ///\n    /// # Safety\n    /// Same contract as [`Value::from_raw`]: a `TAG_PTR` payload must be a\n    /// live Nulang-heap pointer and every tag payload must satisfy its runtime\n    /// invariant.\n    pub unsafe fn from_bits(raw: u64) -> Self {\n        Value { raw }\n    }\n""",
)

# Wrap raw/pointer reconstruction sites throughout Rust sources. This makes
# each call an explicit safety boundary. Untrusted boundary files are tightened
# below after wrapping so the relevant condition is enforced, not just stated.
raw_wrapped = 0
bits_wrapped = 0
ptr_wrapped = 0
for path in list((ROOT / "src").rglob("*.rs")) + list((ROOT / "crates").rglob("*.rs")):
    raw_wrapped += wrap_value_calls(path, "from_raw")
    bits_wrapped += wrap_value_calls(path, "from_bits")
    ptr_wrapped += wrap_value_calls(path, "ptr")

# C callers can fabricate the repr(C) raw field, so reject pointer-tagged
# values before crossing back into the host Value domain.
c_api = ROOT / "src/ffi/c_api.rs"
replace_once(
    c_api,
    """impl From<NulangValue> for Value {\n    fn from(value: NulangValue) -> Self {\n        unsafe { Value::from_bits(value.raw) }\n    }\n}\n""",
    """impl From<NulangValue> for Value {\n    fn from(value: NulangValue) -> Self {\n        if (value.raw & crate::value_layout::TAG_MASK) == crate::value_layout::TAG_PTR {\n            return Value::nil();\n        }\n        // SAFETY: the public C boundary rejects host-pointer tags above. The\n        // remaining immediate tags/floats do not authorize host dereferences.\n        unsafe { Value::from_bits(value.raw) }\n    }\n}\n""",
)

# `WasmRuntime::new` accepts arbitrary wasm bytes. Guest TAG_PTR payloads are
# linear-memory concepts and must never become host pointers.
wasm = ROOT / "src/wasm_runtime.rs"
replace_once(
    wasm,
    """        Ok(unsafe { crate::vm::Value::from_raw(raw as u64) })\n""",
    """        let raw = raw as u64;\n        if (raw & crate::value_layout::TAG_MASK) == crate::value_layout::TAG_PTR {\n            return Err(NuError::runtime_error(\n                \"WASM module returned a host-pointer tag\".to_string(),\n                Span::default(),\n            ));\n        }\n        // SAFETY: guest pointer tags are rejected above. Other guest values\n        // remain opaque immediates/floats; TAG_STRING is a guest-memory offset\n        // consumed only through `string_value`.\n        Ok(unsafe { crate::vm::Value::from_raw(raw) })\n""",
)
replace_once(
    wasm,
    """            cargs.push(unsafe { crate::vm::Value::from_bits(args[i] as u64) });\n""",
    """            let raw = args[i] as u64;\n            if (raw & crate::value_layout::TAG_MASK) == crate::value_layout::TAG_PTR {\n                cargs.push(crate::vm::Value::nil());\n            } else {\n                // SAFETY: guest host-pointer tags are rejected above.\n                cargs.push(unsafe { crate::vm::Value::from_bits(raw) });\n            }\n""",
)

wasmfx = ROOT / "src/wasmfx_runtime.rs"
replace_once(
    wasmfx,
    """            Ok(Ok(raw)) => Ok(unsafe { crate::vm::Value::from_raw(raw as u64) }),\n""",
    """            Ok(Ok(raw)) => {\n                let raw = raw as u64;\n                if (raw & crate::value_layout::TAG_MASK) == crate::value_layout::TAG_PTR {\n                    Err(NuError::runtime_error(\n                        \"WasmFX module returned a host-pointer tag\".to_string(),\n                        crate::types::Span::default(),\n                    ))\n                } else {\n                    // SAFETY: guest host-pointer tags are rejected above.\n                    Ok(unsafe { crate::vm::Value::from_raw(raw) })\n                }\n            }\n""",
)

print(
    f"wrapped raw calls: {raw_wrapped}; wrapped bits calls: {bits_wrapped}; "
    f"wrapped ptr calls: {ptr_wrapped}"
)

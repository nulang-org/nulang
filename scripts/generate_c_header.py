#!/usr/bin/env python3
"""Generate the documented C API header from Rust FFI signatures.

The template owns documentation and ordering. The Rust exports and CType enum
own the types and symbol set. --check is non-mutating and rejects divergence.
This intentionally supports only the narrow, audited FFI type vocabulary.
New types require an explicit mapping and a C ABI review.
"""
import argparse
import difflib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / "include/nulang.h.in"
OUTPUT = ROOT / "include/nulang.h"
C_API = ROOT / "src/ffi/c_api.rs"
MARSHAL = ROOT / "src/ffi/marshal.rs"

RUST_TO_C = {
    "NulangRuntime": "NulangRuntime",
    "NulangValue": "NulangValue",
    "super::marshal::CType": "NulangCType",
    "c_char": "char",
    "c_void": "void",
    "i64": "int64_t",
    "i32": "int",
    "usize": "size_t",
    "f64": "double",
    "bool": "bool",
}


def c_type(raw: str) -> str:
    raw = raw.strip()
    if raw.startswith("*const "):
        return "const " + c_type(raw[len("*const "):]) + " *"
    if raw.startswith("*mut "):
        return c_type(raw[len("*mut "):]) + " *"
    try:
        return RUST_TO_C[raw]
    except KeyError as exc:
        raise ValueError(f"unreviewed C ABI type: {raw}") from exc


def rust_prototypes(source: str) -> dict[str, str]:
    regex = re.compile(
        r'^pub (?:unsafe )?extern "C" fn (nulang_\w+)\(\s*(.*?)\s*\)'
        r'\s*(?:->\s*([^\{]+?))?\s*\{',
        re.M | re.S,
    )
    result = {}
    for match in regex.finditer(source):
        name, params, return_type = match.groups()
        # A public extern function without no_mangle is not linkable by its
        # source-level name. Reject such drift rather than emitting a lie.
        if not re.search(r"(?m)^#\[no_mangle\]\s*$", source[max(0, match.start() - 100):match.start()]):
            raise ValueError(f"C export {name} is missing #[no_mangle]")
        if name in result:
            raise ValueError(f"duplicate C export: {name}")
        args = []
        for parameter in params.split(","):
            if parameter.strip():
                arg, separator, typ = parameter.strip().partition(":")
                if not separator or not arg.strip().isidentifier():
                    raise ValueError(f"unreviewed parameter in {name}: {parameter}")
                args.append(f"{c_type(typ)} {arg.strip()}")
        ret = c_type(return_type) if return_type and return_type.strip() else "void"
        result[name] = f"{ret} {name}({', '.join(args) if args else 'void'});"
    if not result:
        raise ValueError("no C exports found")
    return result


def ctype_enum(source: str) -> str:
    match = re.search(r'pub enum CType\s*\{(.*?)\n\}', source, re.S)
    if match is None:
        raise ValueError("CType enum not found")
    if not re.search(r"#\[repr\(C\)\]\s*#\[derive\([^)]*\)\]\s*$", source[max(0, match.start() - 160):match.start()]):
        raise ValueError("CType must remain #[repr(C)]")
    names = re.findall(r'^\s*([A-Z][a-zA-Z0-9]+),\s*$', match.group(1), re.M)
    if not names:
        raise ValueError("no CType enum variants found")
    # The current ABI relies on the implicit repr(C) discriminants. For
    # explicit assigned discriminants, update this generator deliberately.
    return (
        "typedef enum {\n"
        + "".join(f"    NULANG_CTYPE_{name.upper()} = {i},\n" for i, name in enumerate(names))
        + "} NulangCType;"
    )


def generate() -> str:
    source = C_API.read_text()
    template = TEMPLATE.read_text()
    exports = rust_prototypes(source)
    names = re.findall(r'@NULANG_PROTO_(nulang_\w+)@', template)
    if len(names) != len(set(names)) or set(names) != set(exports):
        missing = sorted(set(exports) - set(names))
        stale = sorted(set(names) - set(exports))
        raise ValueError(f"header/API mismatch; missing placeholders: {missing}; stale: {stale}")
    for name in names:
        template = template.replace(f"@NULANG_PROTO_{name}@", exports[name])
    return template.replace("@NULANG_CTYPE_ENUM@", ctype_enum(MARSHAL.read_text()))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    args = parser.parse_args()
    try:
        generated = generate()
    except (OSError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    if args.write:
        OUTPUT.write_text(generated)
        print(OUTPUT)
        return 0
    actual = OUTPUT.read_text()
    if generated != actual:
        print("include/nulang.h is stale; run python3 scripts/generate_c_header.py --write", file=sys.stderr)
        sys.stderr.writelines(difflib.unified_diff(
            actual.splitlines(keepends=True),
            generated.splitlines(keepends=True),
            fromfile="include/nulang.h", tofile="generated",
        ))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

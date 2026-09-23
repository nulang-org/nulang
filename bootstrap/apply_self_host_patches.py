#!/usr/bin/env python3
"""Idempotent self-hosting patches for bootstrap/compile_hex.nula.

- Arithmetic low4 + emit_hex (avoid miscompiled bitwise & under pressure)
- emit_word wrapper (let w = instr(...); emit_hex(w)) for shallow emit sites
- emit_cap_step / emit_fnend_step helpers + early reorder after bit17
- comp() body uses emit_word via tail-only replace
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


def apply(src: str) -> str:
    if "fn low4(" not in src:
        src = src.replace(
            "fn low6(x: Int) -> Int {",
            "fn low4(x: Int) -> Int {\n"
            "    let q = x / 16;\n"
            "    x - q * 16\n"
            "}\n\n"
            "fn low6(x: Int) -> Int {",
            1,
        )

    if "low4(w /" not in src:
        src = src.replace(
            """fn emit_hex(w: Int) {
    perform IO.print("" +
        hex_digit((w >> 28) & 0xF) +
        hex_digit((w >> 24) & 0xF) +
        hex_digit((w >> 20) & 0xF) +
        hex_digit((w >> 16) & 0xF) +
        hex_digit((w >> 12) & 0xF) +
        hex_digit((w >> 8) & 0xF) +
        hex_digit((w >> 4) & 0xF) +
        hex_digit(w & 0xF)
    )
}""",
            """fn emit_hex(w: Int) {
    perform IO.print("" +
        hex_digit(low4(w / 0x10000000)) +
        hex_digit(low4(w / 0x1000000)) +
        hex_digit(low4(w / 0x100000)) +
        hex_digit(low4(w / 0x10000)) +
        hex_digit(low4(w / 0x1000)) +
        hex_digit(low4(w / 0x100)) +
        hex_digit(low4(w / 0x10)) +
        hex_digit(low4(w))
    )
}""",
            1,
        )

    block = """// Encode instruction: opcode<<24 | op1<<16 | op2<<8 | op3
fn instr(opcode: Int, op1: Int, op2: Int, op3: Int) -> Int {
    (opcode << 24) + (op1 << 16) + (op2 << 8) + op3
}
"""
    helpers = block + """
fn emit_word(opcode: Int, op1: Int, op2: Int, op3: Int) -> Int {
    let w = instr(opcode, op1, op2, op3);
    emit_hex(w);
    0
}

fn emit_cap_step(closure_reg: Int, slot: Int, is_store: Int) -> Int {
    let cap_reg = 11 + slot;
    if is_store != 0 then emit_word(0x62, closure_reg, slot, cap_reg)
    else emit_word(0x61, slot, cap_reg, 0)
}

fn emit_fnend_step(reg: Int, slot: Int, closure_reg: Int) -> Int {
    let cap_dst = 11 + slot;
    emit_word(0x12, reg, cap_dst, 0);
    emit_word(0x62, closure_reg, slot, cap_dst)
}
"""
    if "fn emit_word(" not in src:
        if block not in src:
            raise SystemExit("instr block not found")
        src = src.replace(block, helpers, 1)

    old_caps = """fn emit_caps(closure_reg: Int, count: Int, slot: Int, is_store: Int) -> Int {
    if slot >= count then 0
    else {
        let cap_reg = 11 + slot;
        if is_store != 0 then {
            emit_hex(instr(0x62, closure_reg, slot, cap_reg))
        } else {
            emit_hex(instr(0x61, slot, cap_reg, 0))
        };
        emit_caps(closure_reg, count, slot + 1, is_store)
    }
}"""
    new_caps = """fn emit_caps(closure_reg: Int, count: Int, slot: Int, is_store: Int) -> Int {
    if slot >= count then 0
    else {
        emit_cap_step(closure_reg, slot, is_store);
        emit_caps(closure_reg, count, slot + 1, is_store)
    }
}"""
    if old_caps in src:
        src = src.replace(old_caps, new_caps, 1)

    old_fnend = """        if is_cap then {
            let cap_dst = 11 + slot;
            emit_hex(instr(0x12, reg, cap_dst, 0));
            emit_hex(instr(0x62, closure_reg, slot, cap_dst));
            emit_fnend_caps(env, elen, idx + 1, slot + 1, closure_reg)"""
    new_fnend = """        if is_cap then {
            emit_fnend_step(reg, slot, closure_reg);
            emit_fnend_caps(env, elen, idx + 1, slot + 1, closure_reg)"""
    if old_fnend in src:
        src = src.replace(old_fnend, new_fnend, 1)

    comp_marker = "// Returns (result_reg << 18) | pos."
    if comp_marker in src:
        pos = src.index(comp_marker)
        src = src[:pos] + re.sub(
            r"emit_hex\(instr\(([^)]+)\)\)", r"emit_word(\1)", src[pos:]
        )

    src = src.replace("} else { };", "} else 0;")

    # Two-bank save_reg avoids mod-56 collisions (ne and ne+56 map to same reg).
    new_let_save = (
        "let ne = nr + elen;\n"
        "                            let save_reg = if ne < 56 then 200 + ne else 143 + (ne - 56);"
    )
    new_fn_save = (
        "let ne = nr + elen;\n"
        "            let save_reg = if ne < 56 then 150 + ne else 93 + (ne - 56);"
    )
    for old, new in (
        ("let save_reg = low8(200 + nr + elen);", new_let_save),
        (
            "let ne = nr + elen;\n"
            "                            let neq = ne / 56;\n"
            "                            let save_reg = 200 + (ne - neq * 56);",
            new_let_save,
        ),
        ("let save_reg = low8(150 + nr + elen);", new_fn_save),
        (
            "let ne = nr + elen;\n"
            "            let neq = ne / 106;\n"
            "            let save_reg = 150 + (ne - neq * 106);",
            new_fn_save,
        ),
    ):
        if old in src:
            src = src.replace(old, new, 1)

    if "fn emit_cap_step" in src and "fn bit17" in src:
        pat = re.compile(r"^fn (\w+)", re.M)
        starts = [m.start() for m in pat.finditer(src)]
        blocks = [
            src[s : (starts[i + 1] if i + 1 < len(starts) else len(src))].rstrip()
            for i, s in enumerate(starts)
        ]
        header = src[: starts[0]]

        def fn_name(b: str) -> str:
            return re.match(r"^fn (\w+)", b).group(1)

        by = {fn_name(b): b for b in blocks}
        early = [
            "hex_digit",
            "remark",
            "emit_hex",
            "instr",
            "emit_word",
            "env_decode",
            "emit_cap_step",
            "emit_fnend_step",
            "emit_caps",
            "emit_fnend_caps",
        ]
        rest = [fn_name(b) for b in blocks if fn_name(b) not in early]
        insert_at = rest.index("bit17") + 1
        order = rest[:insert_at] + [n for n in early if n in by] + rest[insert_at:]
        src = header + "\n\n".join(by[n] for n in order) + "\n"

    return src


def main() -> None:
    path = Path(__file__).resolve().parent / "compile_hex.nula"
    path.write_text(apply(path.read_text()))
    print(f"Patched {path}", file=sys.stderr)


if __name__ == "__main__":
    main()

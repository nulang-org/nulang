#!/usr/bin/env python3
"""disasm_nbc.py — Disassemble a Nulang .nbc artifact.

Parses the header described in src/format/nbc.rs and the opcode layout in
src/bytecode.rs, then prints constants, the function table, and instructions.

Usage:
    python3 bootstrap/disasm_nbc.py <file.nbc> [--funcs] [--start N] [--end N]

  --funcs     Dump only named function bodies (needs debug_functions metadata).
  --start N   First instruction to print (inclusive).
  --end N     Last instruction to print (exclusive).
"""

import json
import struct
import sys
from typing import Any

OPCODES: dict[int, str] = {
    0x00: "Nop", 0x01: "Halt", 0x02: "Panic",
    0x03: "Const0", 0x04: "Const1", 0x05: "Const2", 0x06: "ConstM1",
    0x07: "ConstU", 0x08: "ConstL",
    0x10: "Load", 0x11: "Store", 0x12: "Move", 0x13: "Pop",
    0x14: "Dup", 0x15: "Swap",
    0x20: "IAdd", 0x21: "ISub", 0x22: "IMul", 0x23: "IDiv", 0x24: "IMod",
    0x25: "INeg", 0x26: "IInc", 0x27: "IDec", 0x28: "IPow", 0x29: "Xor",
    0x2A: "Shl", 0x2B: "Shr", 0x2C: "BitAnd", 0x2D: "BitOr",
    0x30: "FAdd", 0x31: "FSub", 0x32: "FMul", 0x33: "FDiv", 0x34: "FNeg",
    0x35: "FMod", 0x36: "IToF", 0x37: "FToI", 0x38: "FToS", 0x39: "FPow",
    0x40: "ICmpEq", 0x41: "ICmpLt", 0x42: "ICmpGt", 0x43: "ICmpLe",
    0x44: "ICmpGe", 0x45: "FCmpEq", 0x46: "FCmpLt", 0x47: "FCmpGt",
    0x48: "SCmpEq", 0x49: "Not", 0x4A: "And", 0x4B: "Or",
    0x50: "Jmp", 0x51: "JmpT", 0x52: "JmpF", 0x53: "Switch",
    0x54: "Call", 0x55: "TailCall", 0x56: "Ret", 0x57: "RetVal",
    0x60: "Closure", 0x61: "CapLoad", 0x62: "CapStore", 0x63: "FreeVar",
    0x64: "ClosureCall",
    0x70: "Alloc", 0x71: "FieldL", 0x72: "FieldS", 0x73: "ArrAlloc",
    0x74: "ArrLoad", 0x75: "ArrStore", 0x76: "ArrLen", 0x77: "TupleMk",
    0x78: "TupleL", 0x79: "RecMk", 0x7A: "RecL", 0x7B: "RecS",
    0x7C: "IsTag", 0x7D: "Unpack", 0x7E: "Copy", 0x7F: "Drop",
    0x9D: "RecCopy",
    0x80: "Spawn", 0x81: "Send", 0x82: "Ask", 0x83: "SelfOp",
    0x84: "Receive", 0x85: "Monitor", 0x86: "Demon", 0x87: "Link",
    0x88: "Unlink", 0x89: "Exit", 0x8A: "Yield", 0x8B: "StateGet",
    0x8C: "StateSet", 0x8D: "Emit", 0x8E: "SignalWait", 0x8F: "ReceiveMatch",
    0x90: "Perform", 0x91: "Handle", 0x92: "Resume", 0x93: "Unwind",
    0x9C: "PerformDirect",
    0xA0: "ReceiveWait", 0xA1: "ReceiveCommit",
    0xB0: "FFICall",
    0xC6: "PerformAsync",
    0xD0: "NodeId", 0xD1: "Migrate", 0xD2: "RSend", 0xD3: "RAsk",
    0xD4: "RSpawn", 0xD5: "Gossip",
    0xE0: "SConcat", 0xE1: "SPrint", 0xE2: "SRead", 0xE3: "FOpen",
    0xE4: "FRead", 0xE5: "FWrite", 0xE6: "FClose", 0xE7: "Print",
    0xF0: "DbgBreak", 0xF1: "DbgPrint", 0xF2: "DbgStack",
    0xF3: "MetaType", 0xF4: "MetaCap",
    0xF5: "SpillLoad", 0xF6: "SpillStore",
}


def decode_u32_be(data: bytes, off: int) -> int:
    return struct.unpack(">I", data[off:off + 4])[0]


def load_nbc(path: str) -> tuple[bytes, dict[str, Any], list[int]]:
    with open(path, "rb") as f:
        data = f.read()
    if len(data) < 48:
        raise ValueError("file too short for header")
    magic = data[0:4]
    if magic != b"NLBC":
        raise ValueError(f"bad magic {magic!r}")
    fmt_version = decode_u32_be(data, 4)
    lang_version = decode_u32_be(data, 8)
    instr_count = decode_u32_be(data, 44)
    instrs = []
    for i in range(instr_count):
        off = 48 + i * 4
        instrs.append(decode_u32_be(data, off))
    meta_off = 48 + instr_count * 4
    meta_len = decode_u32_be(data, meta_off)
    meta = json.loads(data[meta_off + 4:meta_off + 4 + meta_len])
    return data, meta, instrs


def fmt_const(c: Any) -> str:
    if isinstance(c, dict):
        if "Int" in c:
            return f"Int({c['Int']})"
        if "Float" in c:
            return f"Float({c['Float']})"
        if "String" in c:
            return f"String({c['String']!r})"
        if "Bool" in c:
            return f"Bool({c['Bool']})"
        if "Nil" in c:
            return "Nil"
        if "Unit" in c:
            return "Unit"
    return repr(c)


def signed16(hi: int, lo: int) -> int:
    v = (hi << 8) | lo
    if v >= 0x8000:
        v -= 0x10000
    return v


def format_operands(op: int, op1: int, op2: int, op3: int,
                    constants: list[Any], pc: int, fn_table: list[int]) -> tuple[str, str]:
    """Return (operands, extra comment)."""
    extra = ""

    def const(idx: int) -> str:
        if 0 <= idx < len(constants):
            return fmt_const(constants[idx])
        return f"<{idx}>"

    # ConstU / Perform / field-name ops share a u16 constant index in op1:op2.
    if op in (0x07, 0x90, 0x8B, 0x8C, 0x8D, 0x8E, 0x8F, 0xA0, 0x7A, 0x7B,
              0x94, 0x95, 0x98):
        idx = (op1 << 8) | op2
        if op == 0x90:
            return f"eff_const#{idx} -> r{op3}", f" ; {const(idx)}"
        if op == 0x07:
            return f"const#{idx} -> r{op3}", f" ; {const(idx)}"
        if op in (0x7A, 0x7B):
            return f"field#{idx} r{op1} -> r{op3}", f" ; {const(idx)}"
        return f"const#{idx}", f" ; {const(idx)}"

    if op == 0x50:
        off = signed16(op1, op2)
        return f"offset={off}", f" -> {pc + 1 + off}"
    if op in (0x51, 0x52):
        off = signed16(op2, op3)
        return f"r{op1} offset={off}", f" -> {pc + 1 + off}"
    if op == 0x53:
        return f"r{op1} table={(op1 << 8) | op2}", ""
    if op in (0x54, 0x55):
        return f"r{op1} argc={op2} -> r{op3}", ""
    if op == 0x60:
        idx = (op1 << 8) | op2
        return f"fn#{idx} -> r{op3}", f" ; pc={fn_table[idx] if idx < len(fn_table) else '?'})"
    if op in (0x61, 0x62):
        return f"r{op1} slot={op2} r{op3}", ""
    if op == 0x64:
        return f"r{op1} argc={op2} -> r{op3}", ""
    if op in (0x70,):
        return f"size={op1} type={op2} -> r{op3}", ""
    if op in (0x71, 0x72):
        return f"r{op1} field={op2} r{op3}", ""
    if op in (0x73, 0x77, 0x79):
        return f"len={op1} -> r{op2}", ""
    if op in (0x74, 0x75):
        return f"r{op1}[r{op2}] -> r{op3}", ""
    if op in (0x76, 0x7C, 0x7D, 0x49):
        return f"r{op1} -> r{op3}", ""
    if op == 0x12:
        return f"r{op1} -> r{op2}", ""
    if op in (0x10, 0x11, 0x14, 0x15):
        return f"r{op1} -> r{op2}", ""
    if op in (0x20, 0x21, 0x22, 0x23, 0x24, 0x29, 0x2A, 0x2B, 0x2C, 0x2D,
              0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x39,
              0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0xE0):
        return f"r{op1} r{op2} -> r{op3}", ""
    if op in (0x25, 0x26, 0x27, 0x36, 0x37, 0x38, 0xF3, 0xF4):
        return f"r{op2} -> r{op3}", ""
    if op == 0xB0:
        return f"ffi#{(op1 << 8) | op2} -> r{op3}", ""
    if op == 0x91:
        return f"table#{(op1 << 8) | op2}", ""
    if op == 0x9C:
        return f"table={op1} binding={op2} -> r{op3}", ""
    if op == 0xF5:
        return f"spill#{(op1 << 8) | op2} -> r{op3}", ""
    if op == 0xF6:
        return f"r{op1} -> spill#{(op2 << 8) | op3}", ""
    return f"{op1} {op2} {op3}".rstrip(), ""


def print_instr(pc: int, w: int, constants: list[Any], fn_table: list[int]) -> None:
    op = (w >> 24) & 0xFF
    op1 = (w >> 16) & 0xFF
    op2 = (w >> 8) & 0xFF
    op3 = w & 0xFF
    name = OPCODES.get(op, f"UNKNOWN_0x{op:02X}")
    ops, extra = format_operands(op, op1, op2, op3, constants, pc, fn_table)
    print(f"  {pc:5}: {w:08x}  {name:14s} {ops:<28s}{extra}")


def main() -> None:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        sys.exit(1)
    path = sys.argv[1]
    funcs_only = "--funcs" in sys.argv
    start = None
    end = None
    for i, arg in enumerate(sys.argv):
        if arg == "--start" and i + 1 < len(sys.argv):
            start = int(sys.argv[i + 1])
        if arg == "--end" and i + 1 < len(sys.argv):
            end = int(sys.argv[i + 1])

    data, meta, instrs = load_nbc(path)
    constants = meta.get("constants", [])
    fn_table = meta.get("function_table", [])
    local_counts = meta.get("function_local_counts", [])
    debug_fns = meta.get("debug_functions", [])
    entry_point = meta.get("entry_point")

    print(f"file: {path}")
    print(f"format={1} lang={1} instructions={len(instrs)} "
          f"constants={len(constants)} functions={len(fn_table)} entry={entry_point}")
    print()
    print("--- Constants ---")
    for idx, c in enumerate(constants):
        print(f"  #{idx:4} {fmt_const(c)}")
    print()

    print("--- Function table ---")
    fn_ranges: list[tuple[str, int, int]] = []
    if debug_fns:
        for d in debug_fns:
            name = d.get("name", "?")
            off = d.get("code_offset", 0)
            length = d.get("code_len", 0)
            print(f"  {name}: pc={off} len={length}")
            fn_ranges.append((name, off, length))
    else:
        for fi, off in enumerate(fn_table):
            nxt = fn_table[fi + 1] if fi + 1 < len(fn_table) else len(instrs)
            lc = local_counts[fi] if fi < len(local_counts) else "?"
            print(f"  fn{fi}: pc={off}..{nxt} locals={lc}")
            fn_ranges.append((f"fn{fi}", off, nxt - off))
    print()

    if funcs_only:
        for name, off, length in fn_ranges:
            if length <= 0:
                continue
            print(f"\n=== {name} (pc {off}..{off + length}) ===")
            for pc in range(off, min(off + length, len(instrs))):
                print_instr(pc, instrs[pc], constants, fn_table)
        return

    if start is None:
        start = 0
    if end is None or end > len(instrs):
        end = len(instrs)
    print(f"--- Instructions [{start}:{end}] ---")
    for pc in range(start, end):
        print_instr(pc, instrs[pc], constants, fn_table)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""disasm_nbc2.py — Decode a .nbc artifact and print a readable disassembly
with function boundaries and constant references.

Usage:
    python3 bootstrap/disasm_nbc2.py <file.nbc> [> disasm.txt]
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
    0x9C: "PerformDirect", 0x9D: "RecCopy",
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


def load_nbc(path: str) -> tuple[dict[str, Any], list[int]]:
    with open(path, "rb") as f:
        data = f.read()
    if len(data) < 48:
        raise ValueError("file too short for header")
    magic = data[0:4]
    if magic != b"NLBC":
        raise ValueError(f"bad magic {magic!r}")
    fmt_version = decode_u32_be(data, 4)
    lang_version = decode_u32_be(data, 8)
    source_hash = data[12:44]
    instr_count = decode_u32_be(data, 44)
    instrs = []
    for i in range(instr_count):
        off = 48 + i * 4
        instrs.append(decode_u32_be(data, off))
    meta_off = 48 + instr_count * 4
    meta_len = decode_u32_be(data, meta_off)
    meta_bytes = data[meta_off + 4:meta_off + 4 + meta_len]
    meta = json.loads(meta_bytes)
    meta["__header__"] = {
        "format_version": fmt_version,
        "language_version": lang_version,
        "source_hash": source_hash.hex() if source_hash != b"\x00" * 32 else None,
        "instr_count": instr_count,
    }
    return meta, instrs


def fmt_const(c: Any) -> str:
    if isinstance(c, dict):
        if "Int" in c:
            return f"Int({c['Int']})"
        if "Float" in c:
            return f"Float({c['Float']})"
        if "String" in c:
            return f'String({c["String"]!r})'
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


def disasm(path: str) -> None:
    meta, instrs = load_nbc(path)
    header = meta.pop("__header__")
    constants = meta.get("constants", [])
    fn_table = meta.get("function_table", [])
    exports = meta.get("exports", [])
    entry_point = meta.get("entry_point")

    print(f"magic=NLBC format_version={header['format_version']} "
          f"language_version={header['language_version']}")
    print(f"source_hash={header['source_hash'] or 'none'}")
    print(f"instr_count={header['instr_count']}")
    print(f"name={meta.get('name', '?')}")
    print(f"function_table={fn_table}")
    print(f"exports={exports}")
    print(f"entry_point={entry_point}")

    if constants:
        print("constants:")
        for i, c in enumerate(constants):
            print(f"  #{i:3} {fmt_const(c)}")
    else:
        print("constants: (none)")

    # Map instruction offset -> function index
    fn_by_offset: dict[int, int] = {}
    for idx, off in enumerate(fn_table):
        fn_by_offset[off] = idx

    print("instructions:")
    for i, w in enumerate(instrs):
        if i in fn_by_offset:
            print(f"\n; === fn #{fn_by_offset[i]} @ {i} ===")
        op = (w >> 24) & 0xFF
        op1 = (w >> 16) & 0xFF
        op2 = (w >> 8) & 0xFF
        op3 = w & 0xFF
        name = OPCODES.get(op, f"UNKNOWN_0x{op:02X}")
        extra = ""
        if op in (0x07, 0x90, 0x9C):
            idx = (op1 << 8) | op2
            c = constants[idx] if 0 <= idx < len(constants) else None
            extra = f" ; const #{idx} = {fmt_const(c)}" if c is not None else f" ; const #{idx} (out of range)"
        elif op == 0x50:
            off = signed16(op1, op2)
            target = i + 1 + off
            extra = f" ; -> {target}"
        elif op in (0x51, 0x52):
            off = signed16(op2, op3)
            target = i + 1 + off
            extra = f" ; r{op1} ? -> {target}"
        elif op == 0x60:
            idx = (op1 << 8) | op2
            extra = f" ; fn #{idx} (offset {fn_table[idx] if 0 <= idx < len(fn_table) else '?'})"
        elif op == 0x54:
            idx = (op1 << 8) | op2
            extra = f" ; fn #{idx}"
        elif op == 0x64:
            extra = f" ; closure r{op1} argc={op2}"
        print(f"  {i:5}: {w:08x}  {name:12} r{op1:<3} r{op2:<3} r{op3:<3}{extra}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("usage: disasm_nbc2.py <file.nbc>", file=sys.stderr)
        sys.exit(1)
    disasm(sys.argv[1])

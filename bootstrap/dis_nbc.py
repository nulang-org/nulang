#!/usr/bin/env python3
"""dis_nbc.py — decode a .nbc artifact and print per-function disassembly."""
import json
import struct
import sys
from pathlib import Path

# Parse opcode names out of src/bytecode.rs if available.
def build_opcode_map():
    src = Path(__file__).parent.parent / "src" / "bytecode.rs"
    names = {}
    if src.exists():
        for line in src.read_text().splitlines():
            if "=" in line and "0x" in line and not line.strip().startswith("//"):
                # e.g. "    Load = 0x10,  // Load from local..."
                try:
                    name_part, rest = line.split("=", 1)
                    name = name_part.strip()
                    hex_part = rest.split(",", 1)[0].strip()
                    opcode = int(hex_part, 16)
                    names[opcode] = name
                except Exception:
                    pass
    return names

OPCODES = build_opcode_map()

def decode_instr(encoded):
    opcode = (encoded >> 24) & 0xFF
    op1 = (encoded >> 16) & 0xFF
    op2 = (encoded >> 8) & 0xFF
    op3 = encoded & 0xFF
    imm16 = encoded & 0xFFFF
    simm16 = struct.unpack(">h", struct.pack(">H", imm16))[0]
    return opcode, op1, op2, op3, imm16, simm16

def disassemble(path):
    data = Path(path).read_bytes()
    if len(data) < 48 or data[:4] != b"NLBC":
        print(f"Not a .nbc artifact: {path}")
        sys.exit(1)
    instr_count = struct.unpack(">I", data[44:48])[0]
    instrs = []
    for i in range(instr_count):
        off = 48 + i * 4
        instrs.append(struct.unpack(">I", data[off:off+4])[0])
    meta_len = struct.unpack(">I", data[48 + instr_count*4 : 52 + instr_count*4])[0]
    meta_off = 52 + instr_count*4
    meta = json.loads(data[meta_off:meta_off+meta_len])
    func_table = meta.get("function_table", [])
    local_counts = meta.get("function_local_counts", [])
    debug = meta.get("debug_functions", [])
    constants = meta.get("constants", [])

    print(f"Module: {meta.get('name', '?')}")
    print(f"Instructions: {instr_count}  Functions: {len(func_table)}")
    print(f"Constants: {len(constants)}")
    for i, c in enumerate(constants):
        print(f"  const[{i}] = {c}")
    print()

    names_by_pc = {}
    for i, pc_bytes in enumerate(func_table):
        name = debug[i].get("name", f"fn{i}") if i < len(debug) else f"fn{i}"
        names_by_pc[pc_bytes] = name
        local_count = local_counts[i] if i < len(local_counts) else "?"
        print(f"function {name} @ pc={pc_bytes} (idx={pc_bytes//4})  locals={local_count}")
        end_bytes = func_table[i+1] if i + 1 < len(func_table) else instr_count * 4
        for idx in range(pc_bytes // 4, end_bytes // 4):
            enc = instrs[idx]
            opcode, op1, op2, op3, imm16, simm16 = decode_instr(enc)
            oname = OPCODES.get(opcode, f"OP_{opcode:02x}")
            extra = ""
            if oname in ("Jmp", "JmpT", "JmpF"):
                target = idx + simm16
                extra = f" -> {target}"
            print(f"  {idx:04x}: {enc:08x}  {oname:10} r{op1:<3} r{op2:<3} r{op3:<3} ; imm={imm16} simm={simm16}{extra}")
        print()

if __name__ == "__main__":
    disassemble(sys.argv[1])

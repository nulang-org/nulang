#!/usr/bin/env python3
"""hex2nbc.py — Convert hex bytecode text to .nbc binary.

Reads hex text output from fixup_hex.py (or compile_hex.nula) and writes
a .nbc binary file that the Nulang VM can load and run.

Usage:
  nulang bootstrap/compile_hex.nula < expr | python3 bootstrap/fixup_hex.py | python3 bootstrap/hex2nbc.py > out.nbc
"""

import sys, re, json, struct


_HEX_WORD = re.compile(r"^[0-9a-fA-F]{8}$")


def parse_hex_word(line: str) -> int | None:
    """Parse one instruction word, repairing host mis-emit of '()' for hex 0."""
    s = line.strip()
    if _HEX_WORD.match(s):
        return int(s, 16)
    if "()" in s:
        repaired = s.replace("()", "0")
        if _HEX_WORD.match(repaired):
            return int(repaired, 16)
    return None


def main():
    lines = sys.stdin.readlines()

    # Parse hex instructions, constant pool, and function table markers
    instructions = []
    pool = []
    fn_table = [0]  # entry point is always at instruction 0
    
    for i, line in enumerate(lines):
        s = line.strip()
        if not s: continue
        if s.startswith("; constant pool:"):
            try:
                inside = s.split("[", 1)[1].rsplit("]", 1)[0]
                pool = []
                for x in inside.split(","):
                    x = x.strip()
                    if not x:
                        continue
                    if x.startswith('"') and x.endswith('"'):
                        pool.append(("str", x[1:-1]))
                    else:
                        pool.append(("int", int(x)))
            except: pass
        elif s.startswith("; FN_START"):
            # Next non-comment line is the start of a function body
            fn_table.append(len(instructions))
        elif s.startswith(";"):
            continue
        else:
            word = parse_hex_word(s)
            if word is not None:
                instructions.append(word)
            else:
                print(f"Error: malformed hex instruction {repr(s)}", file=sys.stderr)
                sys.exit(1)

    if not instructions:
        print("Error: no hex instructions found", file=sys.stderr)
        sys.exit(1)

    # Build constant pool JSON
    consts = []
    for entry in pool:
        if entry[0] == "int":
            consts.append({"Int": entry[1]})
        else:
            consts.append({"String": entry[1]})

    # Build .nbc binary
    magic = b"NLBC"
    header = struct.pack(">4sII32sI", magic, 1, 1, b"\x00" * 32, len(instructions))
    sys.stdout.buffer.write(header)

    for w in instructions:
        sys.stdout.buffer.write(struct.pack(">I", w))

    meta = {
        "name": "main", "constants": consts, "instructions": [],
        "behaviors": [], "function_table": fn_table, "exports": [],
        "entry_point": None, "handler_tables": [], "actor_metadata": [],
        "foreign_functions": [], "tools": [],
    }
    mb = json.dumps(meta).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(mb)))
    sys.stdout.buffer.write(mb)


if __name__ == "__main__":
    main()

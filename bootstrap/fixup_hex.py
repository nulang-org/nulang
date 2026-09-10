#!/usr/bin/env python3
"""fixup_hex.py — Patch placeholder offsets and build constant pool.

Reads the marked hex output from compile_hex.nula (stdin) and produces
corrected hex output (stdout) with proper JmpF/Jmp offsets and ConstU indices.

Markers:
  ; JmpF -> else   — next line is JmpF, target = line after ; else:
  ; Jmp -> end     — next line is Jmp, target = ; end: or end of input
  ; then: / else: / end: — branch boundaries
  ; const N        — next ConstU loads constant value N
"""

import sys
import re


def parse_hex(s: str) -> int:
    return int(s.strip(), 16)


def format_hex(w: int) -> str:
    return f"{w & 0xFFFFFFFF:08x}"


def instr(opcode: int, op1: int, op2: int, op3: int) -> int:
    return (opcode << 24) | (op1 << 16) | (op2 << 8) | op3


def patch_jmpf(word: int, offset: int) -> int:
    cond = (word >> 16) & 0xFF
    return instr(0x52, cond, (offset >> 8) & 0xFF, offset & 0xFF)


def patch_jmp(word: int, offset: int) -> int:
    return instr(0x50, (offset >> 8) & 0xFF, offset & 0xFF, 0)


def patch_constu(word: int, idx: int) -> int:
    dst = word & 0xFF
    return instr(0x07, (idx >> 8) & 0xFF, idx & 0xFF, dst)


def patch_perform(word: int, idx: int) -> int:
    dst = word & 0xFF
    return instr(0x90, (idx >> 8) & 0xFF, idx & 0xFF, dst)


def fixup(lines: list[str]) -> list[str]:
    # First pass: collect instructions and markers
    instr_lines = []  # (line_idx, word)
    markers = {}      # line_idx -> marker_text
    
    for i, line in enumerate(lines):
        s = line.strip()
        if not s:
            continue
        if s.startswith(";"):
            markers[i] = s
        elif re.match(r'^[0-9a-fA-F]{8}$', s):
            instr_lines.append((i, parse_hex(s)))
    
    line_to_ic = {li: ic for ic, (li, _) in enumerate(instr_lines)}
    
    # Find jump markers and their instruction positions
    jmpf_info = []  # (line_idx, ic)
    jmp_info = []   # (line_idx, ic)
    
    for li, marker in sorted(markers.items()):
        if "JmpF" in marker:
            for check_li in range(li + 1, len(lines)):
                if check_li in line_to_ic:
                    jmpf_info.append((check_li, line_to_ic[check_li]))
                    break
        elif marker in ("; Jmp -> end", "; Jmp -> or_end", "; Jmp -> and_end", "; Jmp -> fn_end"):
            for check_li in range(li + 1, len(lines)):
                if check_li in line_to_ic:
                    jmp_info.append((check_li, line_to_ic[check_li]))
                    break
    
    else_markers  = [li for li, m in markers.items() if m.startswith("; else:")]
    end_markers   = [li for li, m in markers.items() if m.startswith("; end:")]
    and_end_markers = [li for li, m in markers.items() if m.startswith("; and_end:")]
    or_right_markers = [li for li, m in markers.items() if m.startswith("; or_right:")]
    or_end_markers = [li for li, m in markers.items() if m.startswith("; or_end:")]
    fn_end_markers = [li for li, m in markers.items() if m.startswith("; fn_end:")]
    fn_start_markers = [li for li, m in markers.items() if m.startswith("; FN_START")]

    patched = {}  # line_idx -> new word

    # Stack-based matching for nested if/else/end blocks.
    # Each `if` emits, in order: JmpF -> else, [then_body], Jmp -> end,
    # else:, [else_body], end:.  Nested ifs appear inside the then/else
    # bodies, so a flat "next marker" search pairs outer jumps with inner
    # labels.  We use a stack of pending if blocks: a JmpF starts a block,
    # the matching else: pairs the JmpF, and the matching end: pairs the
    # Jmp -> end.
    sorted_markers = sorted(markers.items())
    if_stack = []  # each entry: {"jmpf": (li, ic), "jmp": (li, ic) | None}

    for li, marker in sorted_markers:
        if marker == "; JmpF -> else":
            instr_li = li + 1
            if instr_li in line_to_ic:
                if_stack.append({"jmpf": (li, instr_li, line_to_ic[instr_li]), "jmp": None})
        elif marker == "; Jmp -> end":
            if if_stack:
                instr_li = li + 1
                if instr_li in line_to_ic:
                    if_stack[-1]["jmp"] = (li, instr_li, line_to_ic[instr_li])
        elif marker == "; else:":
            if if_stack:
                block = if_stack[-1]
                _, jf_li, jf_ic = block["jmpf"]
                for check_li in range(li + 1, len(lines)):
                    if check_li in line_to_ic:
                        target_ic = line_to_ic[check_li]
                        break
                else:
                    target_ic = None
                if target_ic is not None:
                    offset = target_ic - jf_ic
                    old_word = [w for l, w in instr_lines if l == jf_li][0]
                    patched[jf_li] = patch_jmpf(old_word, offset)
        elif marker == "; end:":
            if if_stack:
                block = if_stack.pop()
                if block["jmp"] is not None:
                    _, jp_li, jp_ic = block["jmp"]
                    for check_li in range(li + 1, len(lines)):
                        if check_li in line_to_ic:
                            target_ic = line_to_ic[check_li]
                            break
                    else:
                        target_ic = len(instr_lines)
                    offset = target_ic - jp_ic
                    old_word = [w for l, w in instr_lines if l == jp_li][0]
                    patched[jp_li] = patch_jmp(old_word, offset)

    # Patch JmpF for and/or (non-if) structures with a flat next-marker search.
    for jf_li, jf_ic in reversed(jmpf_info):
        marker_li = jf_li - 1
        marker = markers.get(marker_li, "")
        if marker != "; JmpF -> else":
            target_ic = None
            if "and_end" in marker:
                for em in and_end_markers:
                    if em > jf_li:
                        for check_li in range(em + 1, len(lines)):
                            if check_li in line_to_ic:
                                target_ic = line_to_ic[check_li]
                                break
                        break
            elif "or_right" in marker:
                for em in or_right_markers:
                    if em > jf_li:
                        for check_li in range(em + 1, len(lines)):
                            if check_li in line_to_ic:
                                target_ic = line_to_ic[check_li]
                                break
                        break
            if target_ic is not None:
                offset = target_ic - jf_ic
                old_word = [w for li, w in instr_lines if li == jf_li][0]
                patched[jf_li] = patch_jmpf(old_word, offset)

    # Track which fn_ends have been consumed to correctly match
    # each function-body-skip Jmp with its corresponding fn_end.
    # Inner fns consume their fn_end first (reversed iteration order).
    available_fn_ends = sorted(fn_end_markers)

    def consume_next_fn_end_after(jp_li):
        for i, em in enumerate(available_fn_ends):
            if em > jp_li:
                del available_fn_ends[i]
                for check_li in range(em + 1, len(lines)):
                    if check_li in line_to_ic:
                        return line_to_ic[check_li]
                break
        return None

    # Patch remaining Jmp offsets (fn_end, or_end, and any Jmp -> end not
    # handled by the if-stack, e.g. orphaned jumps at top level).
    for jp_li, jp_ic in reversed(jmp_info):
        if jp_li in patched:
            continue
        target_ic = None
        marker_text = markers.get(jp_li - 1, "")
        if "fn_end" in marker_text:
            target_ic = consume_next_fn_end_after(jp_li)
        if target_ic is None:
            for em in or_end_markers:
                if em > jp_li:
                    for check_li in range(em + 1, len(lines)):
                        if check_li in line_to_ic:
                            target_ic = line_to_ic[check_li]
                            break
                    break
        if target_ic is None:
            target_ic = len(instr_lines)
        offset = target_ic - jp_ic
        old_word = [w for li, w in instr_lines if li == jp_li][0]
        patched[jp_li] = patch_jmp(old_word, offset)
    
    # Patch Closure function indices from FN_START markers
    fn_indices = {}
    for idx, li in enumerate(sorted(fn_start_markers)):
        fn_indices[li] = idx + 1  # 0 is entry point
    
    # Match fn_starts to fn_ends using nesting (stack-based).
    # The first fn_start pairs with the LAST fn_end at the same nesting level.
    all_markers = [(li, 'start') for li in fn_start_markers] + [(li, 'end') for li in fn_end_markers]
    all_markers.sort()
    stack = []  # pending fn_starts
    fn_pairs = []  # (start_li, end_li)
    for li, kind in all_markers:
        if kind == 'start':
            stack.append(li)
        else:
            if stack:
                start = stack.pop()
                fn_pairs.append((start, li))
    # Build start_li -> end_li map
    start_to_end = {s: e for s, e in fn_pairs}
    
    for fn_li, fn_idx in sorted(fn_indices.items()):
        em = start_to_end.get(fn_li)
        if em is not None:
            for check_li in range(em + 1, len(lines)):
                if check_li in line_to_ic:
                    word = [w for cli, w in instr_lines if cli == check_li][0]
                    if ((word >> 24) & 0xFF) == 0x60:
                        dst = word & 0xFF
                        patched[check_li] = instr(0x60, (fn_idx >> 8) & 0xFF, fn_idx & 0xFF, dst)
                    break

    # Build constant pool from ; const N and ; const "str" markers
    const_markers = {}  # line_idx -> ("int", value) or ("str", value)
    for li, marker in markers.items():
        if marker.startswith("; const "):
            rest = marker[len("; const "):].strip()
            if rest.startswith('"') and rest.endswith('"') and len(rest) >= 2:
                sval = rest[1:-1]
                const_markers[li] = ("str", sval)
            else:
                try:
                    ival = int(rest)
                    const_markers[li] = ("int", ival)
                except ValueError:
                    pass
    
    pool_lines = []
    if const_markers:
        seen = set()
        pool = []  # list of ("int", val) or ("str", val)
        for li in sorted(const_markers):
            entry = const_markers[li]
            if entry not in seen:
                seen.add(entry)
                pool.append(entry)
        
        val_to_idx = {entry: i for i, entry in enumerate(pool)}
        
        for li, entry in sorted(const_markers.items()):
            for check_li in range(li + 1, len(lines)):
                if check_li in line_to_ic:
                    word = [w for cli, w in instr_lines if cli == check_li][0]
                    op = (word >> 24) & 0xFF
                    if op == 0x07:  # ConstU
                        patched[check_li] = patch_constu(word, val_to_idx[entry])
                    elif op == 0x90:  # Perform
                        patched[check_li] = patch_perform(word, val_to_idx[entry])
                    break
        
        def fmt_entry(e):
            k, v = e
            if k == "int":
                return str(v)
            return '"' + v + '"'
        
        pool_lines = ["; constant pool: [" + ", ".join(fmt_entry(e) for e in pool) + "]"]
    
    # Build output with pool header
    result = []
    inserted_pool = False
    for i, line in enumerate(lines):
        if i in patched:
            result.append(format_hex(patched[i]))
        else:
            result.append(line.rstrip())
        # Insert pool after first non-const comment
        if not inserted_pool and pool_lines and line.strip().startswith(";") and "const" not in line:
            for pline in pool_lines:
                result.append(pline)
            inserted_pool = True
    
    return result


def main():
    lines = sys.stdin.readlines()
    result = fixup(lines)
    for line in result:
        print(line)


if __name__ == "__main__":
    main()

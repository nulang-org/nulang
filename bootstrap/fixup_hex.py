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
        elif "()" in s or re.match(r'^[0-9a-fA-F]{8}$', s):
            # Repair the host compiler's mis-emit of '()' (hex digit for
            # register 10, the closure-arg staging register) to 'a' so these
            # instructions get patched like any other. Each '()' corresponds
            # to a '; 10' marker confirming the digit is 10 ('a').
            lines[i] = s.replace("()", "a")
            instr_lines.append((i, parse_hex(s.replace("()", "a"))))
    
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
        elif marker in ("; Jmp -> end", "; Jmp -> or_end", "; Jmp -> and_end",
                        "; Jmp -> and_cont", "; Jmp -> or_fill", "; Jmp -> or_cont",
                        "; Jmp -> fn_end"):
            for check_li in range(li + 1, len(lines)):
                if check_li in line_to_ic:
                    jmp_info.append((check_li, line_to_ic[check_li]))
                    break

    else_markers  = [li for li, m in markers.items() if m.startswith("; else:")]
    end_markers   = [li for li, m in markers.items() if m.startswith("; end:")]
    and_fill_markers = [li for li, m in markers.items() if m.startswith("; and_fill:")]
    and_cont_markers = [li for li, m in markers.items() if m.startswith("; and_cont:")]
    or_right_markers = [li for li, m in markers.items() if m.startswith("; or_right:")]
    or_fill_markers = [li for li, m in markers.items() if m.startswith("; or_fill:")]
    or_cont_markers = [li for li, m in markers.items() if m.startswith("; or_cont:")]
    fn_end_markers = [li for li, m in markers.items() if m.startswith("; fn_end:")]
    fn_start_markers = [li for li, m in markers.items() if m.startswith("; FN_START")]
    
    patched = {}  # line_idx -> new word
    
    
    # Collect marker line sets for stack-based branch matching.
    if_marker_lines = {li for li, m in markers.items() if m == "; if"}
    and_marker_lines = {li for li, m in markers.items() if m == "; and"}
    or_marker_lines = {li for li, m in markers.items() if m == "; or"}

    # Map JmpF/Jmp instruction lines to their target instruction indices.
    jmpf_targets = {}
    jmp_targets = {}

    if_stack = []
    and_stack = []
    or_stack = []

    def instr_after(li):
        for check_li in range(li + 1, len(lines)):
            if check_li in line_to_ic:
                return line_to_ic[check_li]
        return None

    for li in sorted(markers.keys()):
        m = markers[li]
        if li in if_marker_lines:
            if_stack.append({"jmpfs": []})
        elif li in and_marker_lines:
            and_stack.append({"jmpfs": []})
        elif li in or_marker_lines:
            or_stack.append({"jmpfs": []})
        elif "JmpF" in m:
            # Marker immediately precedes the instruction; record the line after.
            if "and_fill" in m or "and_end" in m:
                if and_stack:
                    jf_li = next((c for c in range(li + 1, len(lines)) if c in line_to_ic), None)
                    if jf_li is not None:
                        and_stack[-1].setdefault("jmpfs", []).append(jf_li)
            elif "or_right" in m:
                if or_stack:
                    jf_li = next((c for c in range(li + 1, len(lines)) if c in line_to_ic), None)
                    if jf_li is not None:
                        or_stack[-1].setdefault("jmpfs", []).append(jf_li)
            else:
                if if_stack:
                    jf_li = next((c for c in range(li + 1, len(lines)) if c in line_to_ic), None)
                    if jf_li is not None:
                        if_stack[-1].setdefault("jmpfs", []).append(jf_li)
        elif "Jmp ->" in m:
            if "fn_end" in m:
                continue  # handled separately below
            if "or_fill" in m:
                if or_stack:
                    or_stack[-1]["fill_jmp"] = li + 1
            elif "or_cont" in m:
                if or_stack:
                    or_stack[-1]["cont_jmp"] = li + 1
            elif "or_end" in m:
                if or_stack:
                    or_stack[-1]["jmp"] = li + 1
            elif "and_cont" in m or "and_end" in m:
                if and_stack:
                    and_stack[-1]["jmp"] = li + 1
            else:
                if if_stack:
                    if_stack[-1]["jmp"] = li + 1
        elif m.startswith("; else:"):
            if if_stack:
                frame = if_stack[-1]
                for jf in frame.get("jmpfs", []):
                    jmpf_targets[jf] = instr_after(li)
        elif m.startswith("; end:"):
            if if_stack:
                frame = if_stack.pop()
                if "jmp" in frame:
                    jmp_targets[frame["jmp"]] = instr_after(li)
        elif m.startswith("; and_fill:"):
            if and_stack:
                frame = and_stack[-1]
                for jf in frame.get("jmpfs", []):
                    jmpf_targets[jf] = instr_after(li)
        elif m.startswith("; and_cont:"):
            if and_stack:
                frame = and_stack.pop()
                if "jmp" in frame:
                    jmp_targets[frame["jmp"]] = instr_after(li)
        elif m.startswith("; or_right:"):
            if or_stack:
                frame = or_stack[-1]
                for jf in frame.get("jmpfs", []):
                    jmpf_targets[jf] = instr_after(li)
        elif m.startswith("; or_fill:"):
            if or_stack:
                frame = or_stack[-1]
                if "fill_jmp" in frame:
                    jmp_targets[frame["fill_jmp"]] = instr_after(li)
        elif m.startswith("; or_cont:"):
            if or_stack:
                frame = or_stack.pop()
                if "cont_jmp" in frame:
                    jmp_targets[frame["cont_jmp"]] = instr_after(li)
                if "jmp" in frame:
                    jmp_targets[frame["jmp"]] = instr_after(li)

    # Apply JmpF patches.
    for jf_li, jf_ic in jmpf_info:
        target_ic = jmpf_targets.get(jf_li)
        if target_ic is not None:
            offset = target_ic - jf_ic
            old_word = [w for li, w in instr_lines if li == jf_li][0]
            patched[jf_li] = patch_jmpf(old_word, offset)

    # Match fn-body-skip Jmps to their OWN fn_end using nesting.  Each
    # fn-body-skip Jmp is immediately followed by its fn's FN_START marker;
    # the skip must land on that fn's matching fn_end (the fn_end that closes
    # the FN_START's nesting level), NOT the first fn_end that happens to
    # follow (which, for nested/curried fns, is an inner child's fn_end).
    # Pair FN_STARTs with FN_ENDs by nesting (stack-based): the innermost
    # fn_end closes the most recent unclosed FN_START.
    all_markers = sorted(
        [(li, 'start') for li in fn_start_markers]
        + [(li, 'end') for li in fn_end_markers]
    )
    stack = []
    start_to_end = {}
    for li, kind in all_markers:
        if kind == 'start':
            stack.append(li)
        else:
            if stack:
                start_li = stack.pop()
                start_to_end[start_li] = li

    # Map each fn-body-skip Jmp (line of its Jmp instruction) to the FN_START
    # marker that immediately follows it.
    jmp_to_fn_start = {}
    for jp_li, _jp_ic in jmp_info:
        marker_text = markers.get(jp_li - 1, "")
        if "fn_end" in marker_text:
            for fs in fn_start_markers:
                if fs > jp_li:
                    jmp_to_fn_start[jp_li] = fs
                    break

    # Patch remaining Jmp offsets (function-body skips and any unmatched ends).
    for jp_li, jp_ic in reversed(jmp_info):
        if jp_li in jmp_targets:
            target_ic = jmp_targets[jp_li]
        else:
            marker_text = markers.get(jp_li - 1, "")
            if "fn_end" in marker_text:
                fs_li = jmp_to_fn_start.get(jp_li)
                em = start_to_end.get(fs_li) if fs_li is not None else None
                target_ic = instr_after(em) if em is not None else len(instr_lines)
            else:
                target_ic = len(instr_lines)
        if target_ic is not None:
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
                    assert op in (0x07, 0x90), f"Expected ConstU/Perform after const marker, got op 0x{op:02x} at line {check_li}"
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

#!/usr/bin/env python3
"""Split the monolithic comp() in bootstrap/compile_hex.nula into top-level helpers.

The host compiler truncates very large functions when producing .nbc output, which
breaks self-hosting (the top-level main() call is dropped).  This script replaces
fn comp() with a small dispatcher and a set of helper shards.  Extraction uses
brace-depth counting so it does not depend on fragile string matching for end
markers.

Because Nulang top-level functions are not mutually recursive across definitions,
every shard receives a leading `comp_rec` parameter that is the recursive
compile function.  comp() itself is defined last and passes itself to the
helpers.
"""
from pathlib import Path

TARGET = Path(__file__).resolve().parent / 'compile_hex.nula'

# Parameter lists for each extracted helper.  `comp_rec` is prepended by the script.
TP = {
    'digit': 'src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, c: Int',
    'paren': 'src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int',
    'string': 'src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int',
    'kw': 'src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, q: Int',
    'rq': 'src: String, pos: Int, len: Int, nr: Int, q: Int',
    'ident': 'src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, q: Int, h: Int',
    'left': 'src: String, pos: Int, len: Int, left: Int, nr: Int, env: String, elen: Int',
    'fn_body': 'src: String, q6: Int, len: Int, no_left: Int, nr: Int, env: String, elen: Int, p: Int, ph: Int',
    'infix': 'src: String, len: Int, pair: Int, min_prec: Int, nr: Int, env: String, elen: Int, no_left: Int',
    'infix_call': 'src: String, len: Int, pair: Int, min_prec: Int, nr: Int, env: String, elen: Int, no_left: Int, c: Int, p: Int, lr: Int',
    'infix_cmp': 'src: String, len: Int, pair: Int, min_prec: Int, nr: Int, env: String, elen: Int, no_left: Int, c: Int, p: Int, lr: Int',
    'infix_andor': 'src: String, len: Int, pair: Int, min_prec: Int, nr: Int, env: String, elen: Int, no_left: Int, c: Int, p: Int, lr: Int',
    'infix_binop': 'src: String, len: Int, pair: Int, min_prec: Int, nr: Int, env: String, elen: Int, no_left: Int, c: Int, p: Int, lr: Int',
}


def find_line(lines, substring, start=0):
    for i in range(start, len(lines)):
        if substring in lines[i]:
            return i
    raise ValueError(f'{substring!r} not found starting at line {start + 1}')


def _scan_for_close(lines, start_idx, start_col):
    """Starting just after the '{' at (start_idx, start_col), find the matching '}'."""
    depth = 1
    for j in range(start_idx, len(lines)):
        col_start = start_col + 1 if j == start_idx else 0
        line = lines[j]
        for k in range(col_start, len(line)):
            ch = line[k]
            if ch == '{':
                depth += 1
            elif ch == '}':
                depth -= 1
                if depth == 0:
                    return j
    raise ValueError(f'no matching close for line {start_idx + 1}, col {start_col}')


def find_comp_bounds(lines):
    """Return (start_index, end_index) of the fn comp() { ... } block."""
    start = next(i for i, l in enumerate(lines) if l.startswith('fn comp('))
    open_col = lines[start].index('{')
    end = _scan_for_close(lines, start, open_col)
    return start, end


def find_matching_close(lines, open_idx):
    """Find the line whose closing brace matches the '{' on open_idx."""
    open_col = lines[open_idx].index('{')
    return _scan_for_close(lines, open_idx, open_col)


def branch_body(lines, open_idx):
    """Return the body lines inside the block opened by open_idx."""
    close_idx = find_matching_close(lines, open_idx)
    return lines[open_idx + 1:close_idx]


def dedent(body_lines):
    """Shift body lines so the minimum non-empty indent becomes 4 spaces."""
    min_indent = None
    for l in body_lines:
        if l.strip():
            indent = len(l) - len(l.lstrip())
            if min_indent is None or indent < min_indent:
                min_indent = indent
    if min_indent is None:
        return ['    ' for _ in body_lines]
    out = []
    for l in body_lines:
        if not l.strip():
            out.append('    ')
        else:
            out.append('    ' + l[min_indent:])
    return out


def mkfn(name, params, body_lines):
    return f'fn {name}({params}) -> Int {{\n' + '\n'.join(dedent(body_lines)) + '\n}'


def replace_comp_calls(text):
    """Replace direct recursive comp(...) calls with comp_rec(...)."""
    import re
    return re.sub(r'\bcomp\s*\(', 'comp_rec(', text)


def wrap_shard(name, key, body_lines):
    """Build a helper with a leading comp_rec parameter and rewritten calls."""
    params = 'comp_rec, ' + TP[key]
    body = '\n'.join(dedent(body_lines))
    body = replace_comp_calls(body)
    return mkfn(name, params, body.splitlines())


def split_alpha_branches(alpha_body):
    """Inside the alpha branch, extract each keyword/ident sub-branch."""
    ranges = {}

    let_open = find_line(alpha_body, 'if h == 20633 then {')
    ranges['comp_kw_let'] = ('kw', branch_body(alpha_body, let_open))

    fn_open = find_line(alpha_body, 'if h == 3884 then {', let_open)
    ranges['comp_kw_fn'] = ('kw', branch_body(alpha_body, fn_open))

    if_open = find_line(alpha_body, 'if h == 3987 then {', fn_open)
    ranges['comp_kw_if'] = ('kw', branch_body(alpha_body, if_open))

    true_open = find_line(alpha_body, 'if h == 18036 then {', if_open)
    ranges['comp_kw_true'] = ('rq', branch_body(alpha_body, true_open))

    false_open = find_line(alpha_body, 'if h == 15187 then {', true_open)
    ranges['comp_kw_false'] = ('rq', branch_body(alpha_body, false_open))

    not_open = find_line(alpha_body, 'if h == 23741 then {', false_open)
    ranges['comp_kw_not'] = ('kw', branch_body(alpha_body, not_open))

    np_open = find_line(alpha_body, 'if h == 64877 then {', not_open)
    ranges['comp_kw_nperform'] = ('kw', branch_body(alpha_body, np_open))

    ident_open = find_matching_close(alpha_body, np_open)
    ranges['comp_kw_ident'] = ('ident', branch_body(alpha_body, ident_open))

    return ranges


def split_fn_branch(fn_body):
    """Split the fn keyword branch into parsing (comp_kw_fn) and body emission (comp_fn_body)."""
    marker = 'remark("Jmp -> fn_end");'
    marker_idx = next((i for i, l in enumerate(fn_body) if marker in l), None)
    if marker_idx is None:
        return fn_body, []

    # Find the start of the outer `} else (nr << 18) + p` chain that closes the
    # nested ifs (the first exact match after the marker).  The fn emission body
    # stops at the `}` just before that chain.
    else_chain_start = None
    for i in range(marker_idx + 1, len(fn_body)):
        if fn_body[i].strip() == '} else (nr << 18) + p':
            else_chain_start = i
            break
    if else_chain_start is None:
        return fn_body, []

    indent = len(fn_body[marker_idx]) - len(fn_body[marker_idx].lstrip())
    call_line = ' ' * indent + 'comp_fn_body(comp_rec, src, q6, len, no_left, nr, env, elen, p, ph)'

    parse_body = fn_body[:marker_idx] + [call_line] + fn_body[else_chain_start:]
    emit_body = fn_body[marker_idx:else_chain_start]
    return parse_body, emit_body


def split_infix_branches(infix_body):
    """Split the infix section into call/cmp/andor/binop helpers."""
    call_open = find_line(infix_body, 'if c == 40 then {')
    cmp_open = find_line(infix_body, 'else if c == 61 or c == 33 or c == 60 or c == 62 then {', call_open)
    andor_open = find_line(infix_body, 'else if is_alpha(c) then {', cmp_open)
    # The binop branch is the final top-level `else { ... }`; it shares its
    # opening line with the closing brace of the and/or branch.
    andor_close = find_matching_close(infix_body, andor_open)
    binop_open = andor_close
    if 'else {' not in infix_body[binop_open]:
        raise ValueError('could not find binop else branch')

    call_body = branch_body(infix_body, call_open)
    cmp_body = branch_body(infix_body, cmp_open)
    andor_body = branch_body(infix_body, andor_open)
    binop_body = branch_body(infix_body, binop_open)
    return call_body, cmp_body, andor_body, binop_body


def detect_ranges(lines):
    comp_start, comp_end = find_comp_bounds(lines)

    # Outer if left == no_left then { ... } else { ... }
    outer_then = find_line(lines, 'let pair = if left == no_left then {', comp_start)
    outer_else = find_matching_close(lines, outer_then)

    value_primary_body = branch_body(lines, outer_then)
    left_value_body = branch_body(lines, outer_else)

    # Primary non-alpha branches inside value_primary.
    digit_open = find_line(value_primary_body, 'if is_digit(c) then {')
    paren_open = find_line(value_primary_body, '} else if c == 40 then {', digit_open)
    string_open = find_line(value_primary_body, '} else if c == 34 then {', paren_open)

    # Alpha dispatcher branch.
    alpha_open = find_line(value_primary_body, '} else if is_alpha(c) then {')
    alpha_body = branch_body(value_primary_body, alpha_open)

    ranges = {
        'comp_char_digit': ('digit', branch_body(value_primary_body, digit_open)),
        'comp_char_paren': ('paren', branch_body(value_primary_body, paren_open)),
        'comp_char_string': ('string', branch_body(value_primary_body, string_open)),
    }
    ranges.update(split_alpha_branches(alpha_body))
    ranges['comp_left_value'] = ('left', left_value_body)

    # Split the fn branch into parse + closure-emission pieces.
    fn_parse, fn_emit = split_fn_branch(ranges['comp_kw_fn'][1])
    ranges['comp_kw_fn'] = ('kw', fn_parse)
    ranges['comp_fn_body'] = ('fn_body', fn_emit)

    # Infix section: split into specialised helpers.
    infix_start = find_line(lines, 'let lr = pair >> 18;', comp_start)
    infix_body = lines[infix_start:comp_end]
    call_body, cmp_body, andor_body, binop_body = split_infix_branches(infix_body)
    ranges['comp_infix_call'] = ('infix_call', call_body)
    ranges['comp_infix_cmp'] = ('infix_cmp', cmp_body)
    ranges['comp_infix_andor'] = ('infix_andor', andor_body)
    ranges['comp_infix_binop'] = ('infix_binop', binop_body)

    return comp_start, comp_end, ranges, infix_body


def main():
    lines = TARGET.read_text().splitlines()

    if 'fn comp_char_digit(' in lines or 'fn comp_value_primary(' in lines:
        print('Already split; nothing to do.')
        return

    comp_start, comp_end, ranges, infix_body = detect_ranges(lines)

    order = [
        'comp_char_digit',
        'comp_char_paren',
        'comp_char_string',
        'comp_kw_let',
        'comp_fn_body',
        'comp_kw_fn',
        'comp_kw_if',
        'comp_kw_true',
        'comp_kw_false',
        'comp_kw_not',
        'comp_kw_nperform',
        'comp_kw_ident',
        'comp_left_value',
        'comp_infix_call',
        'comp_infix_cmp',
        'comp_infix_andor',
        'comp_infix_binop',
    ]
    shards = [wrap_shard(name, ranges[name][0], ranges[name][1]) for name in order]

    shards.append('''fn comp_alpha(comp_rec, src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, q: Int, h: Int) -> Int {
    if h == 20633 then comp_kw_let(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)
    else if h == 3884 then comp_kw_fn(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)
    else if h == 3987 then comp_kw_if(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)
    else if h == 18036 then comp_kw_true(comp_rec, src, pos, len, nr, q)
    else if h == 15187 then comp_kw_false(comp_rec, src, pos, len, nr, q)
    else if h == 23741 then comp_kw_not(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)
    else if h == 64877 then comp_kw_nperform(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)
    else comp_kw_ident(comp_rec, src, pos, len, nr, env, elen, no_left, p, q, h)
}''')

    shards.append('''fn comp_value_primary(comp_rec, src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int) -> Int {
    let p = skip_ws(src, pos, len);
    if p >= len then (nr << 18) + p
    else {
        let c = perform String.charAt(src, p);
        if is_digit(c) then comp_char_digit(comp_rec, src, pos, len, nr, env, elen, no_left, p, c)
        else if c == 40 then comp_char_paren(comp_rec, src, pos, len, nr, env, elen, no_left, p)
        else if c == 34 then comp_char_string(comp_rec, src, pos, len, nr, env, elen, no_left, p)
        else if is_alpha(c) then {
            let ident = read_ident(src, p, len, 0);
            let h = ident >> 16;
            let q = low16(ident);
            comp_alpha(comp_rec, src, pos, len, nr, env, elen, no_left, p, q, h)
        } else (nr << 18) + p
    }
}''')

    shards.append('''fn comp_infix(comp_rec, src: String, len: Int, pair: Int, min_prec: Int, nr: Int, env: String, elen: Int, no_left: Int) -> Int {
    let lr = pair >> 18;
    let lp = low17(pair);
    let p = skip_ws(src, lp, len);
    if p >= len then pair
    else {
        let c = perform String.charAt(src, p);
        if c == 40 then comp_infix_call(comp_rec, src, len, pair, min_prec, nr, env, elen, no_left, c, p, lr)
        else if c == 61 or c == 33 or c == 60 or c == 62 then comp_infix_cmp(comp_rec, src, len, pair, min_prec, nr, env, elen, no_left, c, p, lr)
        else if is_alpha(c) then comp_infix_andor(comp_rec, src, len, pair, min_prec, nr, env, elen, no_left, c, p, lr)
        else comp_infix_binop(comp_rec, src, len, pair, min_prec, nr, env, elen, no_left, c, p, lr)
    }
}''')

    shards.append('''fn comp(src: String, pos: Int, len: Int, left: Int, min_prec: Int, nr: Int, env: String, elen: Int) -> Int {
    let no_left = 1 << 40;
    let pair = if left == no_left then comp_value_primary(comp, src, pos, len, nr, env, elen, no_left)
    else comp_left_value(comp, src, pos, len, left, nr, env, elen);
    comp_infix(comp, src, len, pair, min_prec, nr, env, elen, no_left)
}''')

    TARGET.write_text('\n'.join(lines[:comp_start] + [''] + shards + [''] + lines[comp_end + 1:]) + '\n')

    for s in shards:
        print(s.split('(')[0].replace('fn ', ''), len(s))


if __name__ == '__main__':
    main()

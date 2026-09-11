"""Split monolithic comp() in compile_hex.nula for self-hosting (prep-time only)."""
from __future__ import annotations

import re

from split_comp_shards import TP, dedent, detect_ranges, find_line, find_matching_close

def _add_comp_rec_param(sig: str) -> str:
    if sig.startswith('comp_rec,'):
        return sig
    return f'comp_rec, {sig}'


def _wrap_shard(name: str, pk: str, body_lines: list[str]) -> str:
    params = _add_comp_rec_param(TP[pk])
    body = '\n'.join(dedent(body_lines))
    body = re.sub(r'\bcomp\s*\(', 'comp_rec(', body)
    body = re.sub(r'comp_fn_parse\(comp,', 'comp_fn_parse(comp_rec,', body)
    body = re.sub(r'comp_nperform_parse\(comp,', 'comp_nperform_parse(comp_rec,', body)
    return f'fn {name}({params}) -> Int {{\n{body}\n}}'


def split_comp_for_self_host(source: str) -> str:
    """Replace monolithic fn comp with comp_rec-parameterized shards."""
    if 'fn comp_char_digit(' in source or 'fn comp_value_primary(' in source:
        return source
    lines = source.splitlines()
    try:
        comp_start, comp_end, ranges, _infix = detect_ranges(lines)
    except (ValueError, StopIteration):
        return source

    shards: list[str] = []
    for name, (pk, body_lines) in ranges.items():
        shards.append(_wrap_shard(name, pk, body_lines))

    shards.append(
        'fn comp_alpha(comp_rec, src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, q: Int, h: Int) -> Int {\n'
        '    if h == 20633 then comp_kw_let(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)\n'
        '    else if h == 3884 then comp_kw_fn(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)\n'
        '    else if h == 3987 then comp_kw_if(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)\n'
        '    else if h == 6932 then comp_kw_true(comp_rec, src, pos, len, nr, q)\n'
        '    else if h == 15187 then comp_kw_false(comp_rec, src, pos, len, nr, q)\n'
        '    else if h == 23741 then comp_kw_not(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)\n'
        '    else if h == 64877 then comp_kw_nperform(comp_rec, src, pos, len, nr, env, elen, no_left, p, q)\n'
        '    else comp_kw_ident(comp_rec, src, pos, len, nr, env, elen, no_left, p, q, h)\n'
        '}'
    )
    shards.append(
        'fn comp_value_primary(comp_rec, src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int) -> Int {\n'
        '    let p = skip_ws(src, pos, len);\n'
        '    if p >= len then (nr << 18) + p\n'
        '    else {\n'
        '        let c = perform String.charAt(src, p);\n'
        '        if is_digit(c) then comp_char_digit(comp_rec, src, pos, len, nr, env, elen, no_left, p, c)\n'
        '        else if c == 40 then comp_char_paren(comp_rec, src, pos, len, nr, env, elen, no_left, p)\n'
        '        else if c == 34 then comp_char_string(comp_rec, src, pos, len, nr, env, elen, no_left, p)\n'
        '        else if is_alpha(c) then {\n'
        '            let ident = read_ident(src, p, len, 0);\n'
        '            let h = ident >> 16;\n'
        '            let q = low16(ident);\n'
        '            comp_alpha(comp_rec, src, pos, len, nr, env, elen, no_left, p, q, h)\n'
        '        } else (nr << 18) + p\n'
        '    }\n'
        '}'
    )

    shards.append(
        f'fn comp_infix(comp_rec, {TP["infix"]}) -> Int {{\n'
        '    let lr = pair >> 18;\n'
        '    let lp = low17(pair);\n'
        '    let p = skip_ws(src, lp, len);\n'
        '    if p >= len then pair\n'
        '    else {\n'
        '        let c = perform String.charAt(src, p);\n'
        '        if c == 40 then comp_infix_call(comp_rec)(src)(len)(pair)(min_prec)(nr)(env)(elen)(no_left)(c)(p)(lr)\n'
        '        else if c == 61 or c == 33 or c == 60 or c == 62 then comp_infix_cmp(comp_rec)(src)(len)(pair)(min_prec)(nr)(env)(elen)(no_left)(c)(p)(lr)\n'
        '        else if is_alpha(c) then comp_infix_andor(comp_rec)(src)(len)(pair)(min_prec)(nr)(env)(elen)(no_left)(c)(p)(lr)\n'
        '        else comp_infix_binop(comp_rec)(src)(len)(pair)(min_prec)(nr)(env)(elen)(no_left)(c)(p)(lr)\n'
        '    }\n'
        '}'
    )
    shards.append(
        'fn comp(src: String, pos: Int, len: Int, left: Int, min_prec: Int, nr: Int, env: String, elen: Int) -> Int {\n'
        '    let no_left = 1 << 40;\n'
        '    let pair = if left == no_left then comp_value_primary(comp)(src)(pos)(len)(nr)(env)(elen)(no_left)\n'
        '    else comp_left_value(comp)(src)(pos)(len)(left)(nr)(env)(elen);\n'
        '    comp_infix(comp)(src)(len)(pair)(min_prec)(nr)(env)(elen)(no_left)\n'
        '}'
    )

    return '\n'.join(lines[:comp_start] + [''] + shards + [''] + lines[comp_end + 1:]) + '\n'

DEEP_KWS = (
    ('comp_kw_not', 'kw', 'if h == 23741 then {'),
    ('comp_kw_let', 'kw', 'if h == 20633 then {'),
    ('comp_kw_if', 'kw', 'if h == 3987 then {'),
    ('comp_infix_call', 'infix_call', 'if c == 40 then {'),
)





def _param_names(sig: str) -> list[str]:
    return [part.strip().split(':')[0].strip() for part in sig.split(',') if part.strip()]


def split_comp_deep_kws(source: str) -> str:
    """Extract let/if/not/call branches of fn comp into top-level helpers.

    Leaves the rest of comp monolithic so the artifact stays inside the i16
    Jmp budget. Each extracted body is compiled as its own fn, resetting
    let-nesting below the self-compile collision zone.
    """
    if 'fn comp_kw_let(' in source:
        return source
    lines = source.splitlines()
    try:
        comp_start, _comp_end, ranges, _infix = detect_ranges(lines)
    except (ValueError, StopIteration):
        return source

    shards: list[str] = []
    replacements: list[tuple[int, int, str]] = []
    for name, pk, opener in DEEP_KWS:
        if name not in ranges or not ranges[name][1]:
            continue
        shards.append(_wrap_shard(name, pk, ranges[name][1]))
        search_from = comp_start
        if name == 'comp_infix_call':
            search_from = find_line(lines, 'let lr = pair >> 18;', comp_start)
        open_idx = find_line(lines, opener, search_from)
        close_idx = find_matching_close(lines, open_idx)
        indent = ' ' * (len(lines[open_idx]) - len(lines[open_idx].lstrip()) + 4)
        args = ['comp'] + _param_names(TP[pk])
        call = indent + name + '(' + ', '.join(args) + ')'
        replacements.append((open_idx, close_idx, call))

    replacements.sort(key=lambda t: t[0], reverse=True)
    for open_idx, close_idx, call in replacements:
        lines[open_idx + 1:close_idx] = [call]

    return '\n'.join(lines[:comp_start] + [''] + shards + [''] + lines[comp_start:]) + '\n'

def thunk_wrap_let(source: str) -> str:
    """Wrap the let-arm body in-place as (fn() => { body })().

    Captures src/nr/env from fn comp. Does not add a main-chain helper.
    Idempotent if the arm is already thunked.
    """
    lines = source.splitlines()
    try:
        open_idx = find_line(lines, 'if h == 20633 then {')
    except ValueError:
        return source
    close_idx = find_matching_close(lines, open_idx)
    body = lines[open_idx + 1:close_idx]
    if body and '(fn() =>' in body[0]:
        return source
    indent = ' ' * (len(lines[open_idx]) - len(lines[open_idx].lstrip()) + 4)
    wrapped = [indent + '(fn() => {'] + body + [indent + '})()']
    lines[open_idx + 1:close_idx] = wrapped
    return '\n'.join(lines) + '\n'

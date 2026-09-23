#!/usr/bin/env python3
"""Idempotent multiparam fixes for bootstrap/prep_core.py and prep_selfhost.py.

The Rust host miscompiles let-bound helpers unless every function is emitted as
nested single-parameter lambdas and every call site is fully curried.  This
script applies the known-good transforms idempotently.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PREP_CORE = ROOT / 'prep_core.py'
PREP_SELFHOST = ROOT / 'prep_selfhost.py'


def _patch_prep_core(text: str) -> str:
    if 'def curry_call_sites_to_fixpoint' not in text:
        anchor = 'def strip_type_annotations(params_str: str) -> str:'
        insert = '''def curry_call_sites_to_fixpoint(source: str) -> str:
    """Apply curry_call_sites until a fixed point."""
    prev = None
    cur = source
    while cur != prev:
        prev = cur
        cur = curry_call_sites(cur)
    return cur


'''
        if anchor not in text:
            raise SystemExit('prep_core.py: could not insert curry_call_sites_to_fixpoint')
        text = text.replace(anchor, insert + anchor, 1)

    old_repl = """        curried_args = [curry_call_sites(a) for a in args]
        # Skip keywords."""
    new_repl = """        curried_args = [curry_call_sites_to_fixpoint(a) for a in args]
        # Skip keywords."""
    if old_repl in text:
        text = text.replace(old_repl, new_repl, 1)

    old_kw_re = """        if name in KEYWORDS:
            return name + '(' + ', '.join(curried_args) + ')'"""
    new_kw_re = """        if name in KEYWORDS:
            inner = curry_call_sites_to_fixpoint(args_str)
            return name + ' (' + inner + ')'"""
    if old_kw_re in text:
        text = text.replace(old_kw_re, new_kw_re, 1)

    old_curry = """    result = body
    for p in params[1:]:
        result = f'fn({p}) => {result}'"""
    new_curry = """    result = body
    for p in reversed(params[1:]):
        result = f'fn({p}) => {result}'"""
    if old_curry in text:
        text = text.replace(old_curry, new_curry, 1)

    old_tail = "        main_expr = f'{fns[-1][0]}()'"
    new_tail = "        main_expr = f'let _ = {fns[-1][0]}() in 0'"
    if old_tail in text:
        text = text.replace(old_tail, new_tail, 1)

    old_chain = """    expr = transform_perform(main_expr)
    expr = curry_call_sites(expr)
    for name, params, body in reversed(fns):
        body = flatten_blocks(body)
        body = _flatten_block_body(body)
        body = convert_operators(body)
        body = transform_perform(body)
        body = curry_call_sites(body)"""
    new_chain = """    expr = curry_call_sites_to_fixpoint(transform_perform(main_expr))
    for name, params, body in reversed(fns):
        body = flatten_blocks(body)
        body = _flatten_block_body(body)
        body = convert_operators(body)
        body = curry_call_sites_to_fixpoint(transform_perform(body))"""
    if old_chain in text:
        text = text.replace(old_chain, new_chain, 1)

    if 'from prep_selfhost import transform_fns_for_self_host' not in text:
        text = text.replace(
            'import sys\nimport re\n',
            'import sys\nimport re\n\n'
            'from prep_selfhost import transform_fns_for_self_host, rewrite_emit_word_calls\n'
            'from split_comp_for_self_host import split_comp_for_self_host\n',
            1,
        )

    if 'from split_comp_for_self_host import split_comp_for_self_host' not in text:
        text = text.replace(
            'from prep_selfhost import transform_fns_for_self_host, rewrite_emit_word_calls',
            'from prep_selfhost import transform_fns_for_self_host, rewrite_emit_word_calls\n'
            'from split_comp_for_self_host import split_comp_for_self_host',
            1,
        )

    old_main = """def main():
    source = sys.stdin.read()
    source = strip_comments(source)
    fns, main_expr = parse_top_level_fns(source)"""
    new_main = """def main():
    source = sys.stdin.read()
    # Comp sharding adds ~15 let-bindings; keep monolithic comp for now.
    # source = split_comp_for_self_host(source)
    source = strip_comments(source)
    fns, main_expr = parse_top_level_fns(source)"""
    # Disable comp split if a prior patch enabled it.
    text = text.replace(
        '    source = split_comp_for_self_host(source)\n',
        '    # source = split_comp_for_self_host(source)\n',
    )
    if old_main in text:
        text = text.replace(old_main, new_main, 1)

    old_main_body = """    fns, main_expr = parse_top_level_fns(source)
    result = convert_to_let_chain(fns, main_expr)
    # Collapse whitespace"""
    new_main_body = """    fns, main_expr = parse_top_level_fns(source)
    fns = transform_fns_for_self_host(fns)
    result = convert_to_let_chain(fns, main_expr)
    result = rewrite_emit_word_calls(result)
    # Collapse whitespace"""
    if old_main_body in text:
        text = text.replace(old_main_body, new_main_body, 1)

    old_kw = """            if name in KEYWORDS:
                curried_args = [curry_call_sites_to_fixpoint(a) for a in args]
                out.append(name + '(' + ', '.join(curried_args) + ')')
                i = close_i + 1
                continue"""
    new_kw = """            if name in KEYWORDS:
                # `then (expr)` / `else (expr)` are branch syntax, not calls.
                inner = curry_call_sites_to_fixpoint(args_str)
                while out and out[-1].isspace():
                    out.pop()
                if len(out) >= len(name) and ''.join(out[-len(name):]) == name:
                    del out[-len(name):]
                out.append(name + ' (' + inner + ')')
                i = close_i + 1
                continue"""
    if old_kw in text:
        text = text.replace(old_kw, new_kw, 1)

    return text


def _patch_prep_selfhost(text: str) -> str:
    old_transform = """    fns = _inline_emit_cap_helpers(fns)
    fns = _inline_skip_fn_type(fns)
    fns = _inline_nperform_finish(fns)
    fns = _merge_comp_fn_shards(fns)"""
    new_transform = """    fns = _inline_emit_cap_helpers(fns)
    fns = _merge_comp_fn_shards(fns)"""
    if old_transform in text:
        text = text.replace(old_transform, new_transform, 1)

    old_drop = """    drop = {
        body_name, 'comp_nperform_arg0', 'comp_nperform_arg1', 'comp_nperform_arg2',
        'comp_nperform_args', 'comp_nperform_more',
    }"""
    new_drop = """    drop = {
        parse_name, body_name, 'comp_nperform_arg0', 'comp_nperform_arg1', 'comp_nperform_arg2',
        'comp_nperform_args', 'comp_nperform_more',
    }"""
    if old_drop in text:
        text = text.replace(old_drop, new_drop, 1)
    old_emit = """        if name == 'emit_caps':
            body = '0'
        else:
            body = re.sub(r'let\\s+_\\s*=\\s*0\\s*;\\s*', '', body)
        out.append((name, params, body))"""
    new_emit = """        if name == 'emit_caps':
            out.append((name, params, '0'))
            continue
        body = _replace_calls(body, 'emit_caps', '0')
        body = re.sub(r'let\\s+_\\s*=\\s*0\\s*;\\s*', '', body)
        out.append((name, params, body))"""
    if old_emit in text:
        text = text.replace(old_emit, new_emit, 1)

    return text


def main() -> int:
    changed = []
    for path, patcher in ((PREP_CORE, _patch_prep_core), (PREP_SELFHOST, _patch_prep_selfhost)):
        text = path.read_text()
        updated = patcher(text)
        if updated != text:
            path.write_text(updated)
            changed.append(path.name)
    if changed:
        print('patched:', ', '.join(changed))
    else:
        print('already up to date')
    return 0


if __name__ == '__main__':
    sys.exit(main())

#!/usr/bin/env python3
"""Stage-2 fn/nperform shard extractions for bootstrap/compile_hex.nula (idempotent)."""
from __future__ import annotations
import re, sys
from pathlib import Path

_FN_HELPERS = r'''
fn skip_fn_type(src: String, pos: Int, len: Int) -> Int {
    let q5_raw = skip_ws(src, pos, len);
    if q5_raw < len then {
        let c_colon = perform String.charAt(src, q5_raw);
        if c_colon == 58 then {
            let after_colon = skip_ws(src, q5_raw + 1, len);
            let type_id = read_ident(src, after_colon, len, 0);
            low16(type_id)
        } else { q5_raw }
    } else { q5_raw }
}

fn comp_fn_closure(comp_rec, src: String, q6: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, ph: Int, p: Int) -> Int {
    if q6 + 1 < len then {
        let c3 = perform String.charAt(src, q6);
        let c4 = perform String.charAt(src, q6 + 1);
        if c3 == 61 and c4 == 62 then {
            remark("Jmp -> fn_end");
            emit_word(0x50, 0, 0, 0);
            remark("FN_START");
            let param_save = 100 + elen;
            emit_word(0x12, 10, param_save, 0);
            let env_remapped = remap_captures(env, elen, 0, 0);
            let capture_count = (perform String.length(env_remapped) - perform String.length(env)) / 4;
            let env2 = if capture_count > 0 then {
                env_push(env_remapped, low16(ph), param_save)
            } else {
                env_push(env, low16(ph), param_save)
            };
            let elen2 = elen + capture_count + 1;
            let _ = emit_caps(0, capture_count, 0, 0);
            emit_word(0x12, param_save, 11 + capture_count, 0);
            let body = comp_rec(src, q6 + 2, len, no_left, -1, 8, env2, elen2);
            let body_reg = body >> 18;
            let body_end = low17(body);
            emit_word(0x12, body_reg, 0, 0);
            emit_word(0x57, 0, 0, 0);
            remark("fn_end:");
            let closure_reg = nr;
            emit_word(0x60, 0xFF, 0xFF, closure_reg);
            let _ = emit_caps(closure_reg, capture_count, 0, 1);
            (closure_reg << 18) + body_end
        } else (nr << 18) + p
    } else (nr << 18) + p
}

fn comp_fn_parse(comp_rec, src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, q: Int) -> Int {
    let q2 = skip_ws(src, q, len);
    if q2 < len then {
        let c1 = perform String.charAt(src, q2);
        if c1 == 40 then {
            let q3 = skip_ws(src, q2 + 1, len);
            let param_id = read_ident(src, q3, len, 0);
            let ph = param_id >> 16;
            let q4 = low16(param_id);
            let q5 = skip_fn_type(src, q4, len);
            if q5 < len then {
                let c2 = perform String.charAt(src, q5);
                if c2 == 41 then {
                    let q6 = skip_ws(src, q5 + 1, len);
                    comp_fn_closure(comp_rec, src, q6, len, nr, env, elen, no_left, ph, p)
                } else (nr << 18) + p
            } else (nr << 18) + p
        } else (nr << 18) + p
    } else (nr << 18) + p
}

fn nperform_finish(src: String, eff_start: Int, len: Int, dst: Int, end_pos: Int) -> Int {
    let eff_name = read_string(src, eff_start, len, "");
    let quote = perform String.from_char(34);
    remark("const " + quote + eff_name + quote);
    emit_word(0x90, 0, 0, dst);
    (dst << 18) + end_pos
}

fn comp_nperform_body(comp_rec, src: String, eff_start: Int, q5: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int) -> Int {
    if q5 < len then {
        let c3 = perform String.charAt(src, q5);
        let dst = nr;
        if c3 == 41 then {
            nperform_finish(src, eff_start, len, dst, skip_ws(src, q5 + 1, len))
        } else if c3 == 44 then {
            let no_left_np = 1 << 40;
            let p0 = skip_ws(src, q5 + 1, len);
            let inner0 = comp_rec(src, p0, len, no_left_np, 0, nr + 1, env, elen);
            let r0 = inner0 >> 18;
            let e0 = low17(inner0);
            emit_word(0x12, r0, 0, 0);
            let ap0 = skip_ws(src, e0, len);
            if ap0 < len then {
                let cp0 = perform String.charAt(src, ap0);
                if cp0 == 41 then {
                    nperform_finish(src, eff_start, len, dst, skip_ws(src, ap0 + 1, len))
                } else if cp0 == 44 then {
                    let p1 = skip_ws(src, ap0 + 1, len);
                    let inner1 = comp_rec(src, p1, len, no_left_np, 0, nr + 11, env, elen);
                    let r1 = inner1 >> 18;
                    let e1 = low17(inner1);
                    emit_word(0x12, r1, 1, 0);
                    let ap1 = skip_ws(src, e1, len);
                    if ap1 < len then {
                        let cp1 = perform String.charAt(src, ap1);
                        if cp1 == 41 then {
                            nperform_finish(src, eff_start, len, dst, skip_ws(src, ap1 + 1, len))
                        } else if cp1 == 44 then {
                            let p2 = skip_ws(src, ap1 + 1, len);
                            let inner2 = comp_rec(src, p2, len, no_left_np, 0, nr + 21, env, elen);
                            let r2 = inner2 >> 18;
                            let e2 = low17(inner2);
                            emit_word(0x12, r2, 2, 0);
                            let ap2 = skip_ws(src, e2, len);
                            if ap2 < len then {
                                let cp2 = perform String.charAt(src, ap2);
                                if cp2 == 41 then {
                                    nperform_finish(src, eff_start, len, dst, skip_ws(src, ap2 + 1, len))
                                } else (nr << 18) + p
                            } else (nr << 18) + p
                        } else (nr << 18) + p
                    } else (nr << 18) + p
                } else (nr << 18) + p
            } else (nr << 18) + p
        } else (nr << 18) + p
    } else (nr << 18) + p
}

fn comp_nperform_parse(comp_rec, src: String, pos: Int, len: Int, nr: Int, env: String, elen: Int, no_left: Int, p: Int, q: Int) -> Int {
    let q2 = skip_ws(src, q, len);
    if q2 < len then {
        let c1 = perform String.charAt(src, q2);
        if c1 == 40 then {
            let q3 = skip_ws(src, q2 + 1, len);
            if q3 < len then {
                let c2 = perform String.charAt(src, q3);
                if c2 == 34 then {
                    let eff_start = q3 + 1;
                    let q4 = read_string_end(src, q3 + 1, len);
                    let q5 = skip_ws(src, q4, len);
                    comp_nperform_body(comp_rec, src, eff_start, q5, len, nr, env, elen, no_left, p)
                } else (nr << 18) + p
            } else (nr << 18) + p
        } else (nr << 18) + p
    } else (nr << 18) + p
}

'''

def apply(src: str) -> str:
    comp_marker = "\nfn comp(src: String, pos: Int, len: Int, left: Int, min_prec: Int, nr: Int, env: String, elen: Int) -> Int {"
    if "fn comp_fn_parse(" not in src:
        src = src.replace(comp_marker, _FN_HELPERS + comp_marker, 1)
    m = re.search(r'                } else if h == 620 then \{.*?                } else if h == 627 then \{', src, re.S)
    if m and 'comp_fn_parse(comp' not in m.group(0):
        src = src[:m.start()] + '                } else if h == 620 then {\n                    // fn\n                    comp_fn_parse(comp, src, pos, len, nr, env, elen, no_left, p, q)\n                } else if h == 627 then {' + src[m.end():]
    m2 = re.search(r'                } else if h == 64461 then \{.*?                } else \{', src, re.S)
    if m2 and 'comp_nperform_parse(comp' not in m2.group(0):
        src = src[:m2.start()] + '                } else if h == 64461 then {\n                    // nperform("effect.name" [, arg1 [, arg2 [, arg3]]])\n                    comp_nperform_parse(comp, src, pos, len, nr, env, elen, no_left, p, q)\n                } else {' + src[m2.end():]
    if len(src.splitlines()) < 700 or 'fn main()' not in src:
        raise SystemExit('truncated')
    return src

def main():
    path = Path(__file__).resolve().parent / 'compile_hex.nula'
    out = apply(path.read_text())
    path.write_text(out)
    print(f'patched {len(out.splitlines())} lines', file=sys.stderr)

if __name__ == '__main__':
    main()

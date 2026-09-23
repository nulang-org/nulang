#!/usr/bin/env python3
"""Stage-2 env/recursion core splits for bootstrap/compile_hex.nula (idempotent)."""
from __future__ import annotations

import re
import sys
from pathlib import Path

READ_IDENT = """fn read_ident_core(read_rec, src: String, pos: Int, len: Int, h: Int) -> Int {
    if pos >= len then ((low16(h)) << 16) + pos
    else {
        let c = perform String.charAt(src, pos);
        if is_alphanum(c) then {
            let nh = h * 5 + c;
            read_ident(src, pos + 1, len, nh)
        } else ((low16(h)) << 16) + pos
    }
}

fn read_ident(src: String, pos: Int, len: Int, h: Int) -> Int {
    read_ident_core(read_ident, src, pos, len, h)
}"""

OLD_READ_IDENT = """fn read_ident(src: String, pos: Int, len: Int, h: Int) -> Int {
    if pos >= len then ((low16(h)) << 16) + pos
    else {
        let c = perform String.charAt(src, pos);
        if is_alphanum(c) then {
            let nh = h * 5 + c;
            read_ident(src, pos + 1, len, nh)
        } else ((low16(h)) << 16) + pos
    }
}"""

LOOKUP = """fn lookup_core(lk_rec, env: String, elen: Int, h: Int) -> Int {
    if elen <= 0 then 0
    else {
        let v = env_decode(env, (elen - 1) * 4);
        let hh = low16((v >> 8));
        let reg = low8(v);
        if hh == (low16(h)) then reg
        else env_lookup(env, elen - 1, h)
    }
}

fn env_lookup(env: String, elen: Int, h: Int) -> Int {
    lookup_core(env_lookup, env, elen, h)
}"""

OLD_LOOKUP = """fn env_lookup(env: String, elen: Int, h: Int) -> Int {
    if elen <= 0 then 0
    else {
        let v = env_decode(env, (elen - 1) * 4);
        let hh = low16((v >> 8));
        let reg = low8(v);
        if hh == (low16(h)) then reg
        else env_lookup(env, elen - 1, h)
    }
}"""

CAPCOUNT = """fn capcount_core(cc_rec, env: String, elen: Int) -> Int {
    if elen <= 0 then 0
    else {
        let v = env_decode(env, (elen - 1) * 4);
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        let rest = count_captures(env, elen - 1);
        if is_cap then 1 + rest else rest
    }
}

fn count_captures(env: String, elen: Int) -> Int {
    capcount_core(count_captures, env, elen)
}"""

OLD_CAPCOUNT = """fn count_captures(env: String, elen: Int) -> Int {
    if elen <= 0 then 0
    else {
        let v = env_decode(env, (elen - 1) * 4);
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        let rest = count_captures(env, elen - 1);
        if is_cap then 1 + rest else rest
    }
}"""

REMAP = """fn remap_core(rm_rec, env: String, elen: Int, idx: Int, slot: Int) -> String {
    if idx >= elen then env
    else {
        let v = env_decode(env, idx * 4);
        let hh = low16((v >> 8));
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        if is_cap then {
            let pv = ((low16(hh)) << 8) + (low8(11 + slot));
            let c0 = (low6((pv >> 18))) + 1;
            let c1 = (low6((pv >> 12))) + 1;
            let c2 = (low6((pv >> 6))) + 1;
            let c3 = (low6(pv)) + 1;
            let tail = remap_captures(env, elen, idx + 1, slot + 1);
            tail +
                perform String.from_char(c0) +
                perform String.from_char(c1) +
                perform String.from_char(c2) +
                perform String.from_char(c3)
        } else {
            remap_captures(env, elen, idx + 1, slot)
        }
    }
}

fn remap_captures(env: String, elen: Int, idx: Int, slot: Int) -> String {
    remap_core(remap_captures, env, elen, idx, slot)
}"""

OLD_REMAP = """fn remap_captures(env: String, elen: Int, idx: Int, slot: Int) -> String {
    if idx >= elen then env
    else {
        let i = idx * 4;
        let b0 = perform String.charAt(env, i) - 1;
        let b1 = perform String.charAt(env, i + 1) - 1;
        let b2 = perform String.charAt(env, i + 2) - 1;
        let b3 = perform String.charAt(env, i + 3) - 1;
        let v = (b0 << 18) + (b1 << 12) + (b2 << 6) + b3;
        let hh = low16((v >> 8));
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        if is_cap then {
            let pv = ((low16(hh)) << 8) + (low8(11 + slot));
            let c0 = (low6((pv >> 18))) + 1;
            let c1 = (low6((pv >> 12))) + 1;
            let c2 = (low6((pv >> 6))) + 1;
            let c3 = (low6(pv)) + 1;
            let tail = remap_captures(env, elen, idx + 1, slot + 1);
            tail +
                perform String.from_char(c0) +
                perform String.from_char(c1) +
                perform String.from_char(c2) +
                perform String.from_char(c3)
        } else {
            remap_captures(env, elen, idx + 1, slot)
        }
    }
}"""

CAPS_WALK = """fn caps_walk_core(walk_rec, env: String, elen: Int, idx: Int, slot: Int, closure_reg: Int) -> Int {
    if idx >= elen then slot
    else {
        let v = env_decode(env, idx * 4);
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        if is_cap then {
            emit_fnend_step(reg, slot, closure_reg);
            caps_walk(env, elen, idx + 1, slot + 1, closure_reg)
        } else {
            caps_walk(env, elen, idx + 1, slot, closure_reg)
        }
    }
}

fn caps_walk(env: String, elen: Int, idx: Int, slot: Int, closure_reg: Int) -> Int {
    caps_walk_core(caps_walk, env, elen, idx, slot, closure_reg)
}"""

OLD_CAPS_WALK = """fn emit_fnend_caps(env: String, elen: Int, idx: Int, slot: Int, closure_reg: Int) -> Int {
    if idx >= elen then slot
    else {
        let i = idx * 4;
        let b0 = perform String.charAt(env, i) - 1;
        let b1 = perform String.charAt(env, i + 1) - 1;
        let b2 = perform String.charAt(env, i + 2) - 1;
        let b3 = perform String.charAt(env, i + 3) - 1;
        let v = (b0 << 18) + (b1 << 12) + (b2 << 6) + b3;
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        if is_cap then {
            emit_fnend_step(reg, slot, closure_reg);
            emit_fnend_caps(env, elen, idx + 1, slot + 1, closure_reg)
        } else {
            emit_fnend_caps(env, elen, idx + 1, slot, closure_reg)
        }
    }
}"""


ENV_DECODE = """fn env_decode(env: String, i: Int) -> Int {
    let b0 = perform String.charAt(env, i) - 1;
    let b1 = perform String.charAt(env, i + 1) - 1;
    let b2 = perform String.charAt(env, i + 2) - 1;
    let b3 = perform String.charAt(env, i + 3) - 1;
    (b0 << 18) + (b1 << 12) + (b2 << 6) + b3
}

"""

DECODE_ELEN = """        let i = (elen - 1) * 4;
        let b0 = perform String.charAt(env, i) - 1;
        let b1 = perform String.charAt(env, i + 1) - 1;
        let b2 = perform String.charAt(env, i + 2) - 1;
        let b3 = perform String.charAt(env, i + 3) - 1;
        let v = (b0 << 18) + (b1 << 12) + (b2 << 6) + b3;"""

DECODE_IDX = """        let i = idx * 4;
        let b0 = perform String.charAt(env, i) - 1;
        let b1 = perform String.charAt(env, i + 1) - 1;
        let b2 = perform String.charAt(env, i + 2) - 1;
        let b3 = perform String.charAt(env, i + 3) - 1;
        let v = (b0 << 18) + (b1 << 12) + (b2 << 6) + b3;"""

REMAP_INLINE = """fn remap_captures(env: String, elen: Int, idx: Int, slot: Int) -> String {
    if idx >= elen then env
    else {
        let v = env_decode(env, idx * 4);
        let hh = low16((v >> 8));
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        if is_cap then {
            let pv = ((low16(hh)) << 8) + (low8(11 + slot));
            let c0 = (low6((pv >> 18))) + 1;
            let c1 = (low6((pv >> 12))) + 1;
            let c2 = (low6((pv >> 6))) + 1;
            let c3 = (low6(pv)) + 1;
            let tail = remap_captures(env, elen, idx + 1, slot + 1);
            tail +
                perform String.from_char(c0) +
                perform String.from_char(c1) +
                perform String.from_char(c2) +
                perform String.from_char(c3)
        } else {
            remap_captures(env, elen, idx + 1, slot)
        }
    }
}"""

REMAP_ENVPUSH = """fn remap_captures(env: String, elen: Int, idx: Int, slot: Int) -> String {
    if idx >= elen then env
    else {
        let i = idx * 4;
        let b0 = perform String.charAt(env, i) - 1;
        let b1 = perform String.charAt(env, i + 1) - 1;
        let b2 = perform String.charAt(env, i + 2) - 1;
        let b3 = perform String.charAt(env, i + 3) - 1;
        let v = (b0 << 18) + (b1 << 12) + (b2 << 6) + b3;
        let hh = low16((v >> 8));
        let reg = low8(v);
        let is_cap = reg >= 100 and reg < 128;
        if is_cap then {
            let tail = remap_captures(env, elen, idx + 1, slot + 1);
            env_push(tail, hh, 11 + slot)
        } else {
            remap_captures(env, elen, idx + 1, slot)
        }
    }
}"""


def _install_env_decode(src: str) -> str:
    if "fn env_decode(" not in src:
        marker = "\n// Scan env entries oldest→newest and emit Move+CapStore for each capture."
        src = src.replace(marker, "\n" + ENV_DECODE + marker.lstrip("\n"), 1)
    while DECODE_ELEN in src:
        src = src.replace(DECODE_ELEN, "        let v = env_decode(env, (elen - 1) * 4);", 1)
    while DECODE_IDX in src:
        src = src.replace(DECODE_IDX, "        let v = env_decode(env, idx * 4);", 1)
    if REMAP_ENVPUSH in src:
        src = src.replace(REMAP_ENVPUSH, REMAP_INLINE, 1)
    return src


def apply(src: str) -> str:
    src = _install_env_decode(src)
    if len(src.splitlines()) < 700 or "fn main()" not in src:
        raise SystemExit("truncated after env patches")
    return src

def main() -> None:
    path = Path(__file__).resolve().parent / "compile_hex.nula"
    path.write_text(apply(path.read_text()))
    print(f"patched {len(path.read_text().splitlines())} lines")

if __name__ == "__main__":
    main()

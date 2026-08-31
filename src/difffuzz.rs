//! Grammar-based differential program generator + campaign runner.
//!
//! Complements the mutation-based fuzzer in `src/fuzz.rs`: instead of
//! mutating a fixed seed corpus (which spends most iterations on malformed
//! programs that never compile), this module generates *well-formed* Nulang
//! programs from a typed grammar subset, deterministically from a `u64`
//! seed. Every generated program is fed through the cross-backend oracle
//! `fuzz::differential_fuzz_one` (bytecode VM cold, VM with forced JIT
//! tier-up, and AOT native compilation when the program is within AOT's
//! supported subset); any disagreement is a real bug.
//!
//! Grammar coverage:
//!   * int arithmetic incl. 48-bit boundary values (±2^47±1), div/mod with
//!     provably-nonzero divisors, `**`, unary neg
//!   * float arithmetic incl. ±0.0, subnormal-ish magnitudes, NaN-producing
//!     `0.0 / 0.0` at safe positions
//!   * strings: literals, `+` concat, `==`/`!=` comparison, `"x" + int`
//!     coercion
//!   * bools: comparisons, `and`/`or`/`not`
//!   * arrays: literals, guarded indexing, tracked literal lengths
//!   * records: literals, field access, functional update `{r .. f = v}`
//!   * control flow: `if`/`then`/`else`, bounded `while` loops over `var`
//!     accumulators, `for x in [..]` loops, `match` on int literals
//!   * functions and closures: top-level `fn` decls, `fn(x) { .. }`
//!     lambdas with captures, recursion (bounded)
//!   * effects: a bounded `effect Tick { next: Int -> Int }` perform/handle
//!     wrapper around a `while` loop (the `resume` value crossing the JIT
//!     yield boundary is a known-hot correctness area)
//!
//! Determinism: `generate_program(seed)` is a pure function of `seed`.
//! Crasher files persist the seed so any divergence reproduces exactly.
//!
//! See docs/DIFFERENTIAL_FUZZING.md for design and triage documentation.

use crate::fuzz::{differential_fuzz_one, DiffOutcome, XorShift64};

// ---------------------------------------------------------------------------
// Typed expression generator
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Int,
    Float,
    Bool,
    Str,
}

/// A generated function signature we can call: name, param types, return
/// type. Generated functions are pure expressions, so calls are safe
/// anywhere.
struct FnSig {
    name: String,
    params: Vec<Ty>,
    ret: Ty,
    /// Body recurses on the first (Int) param with `p0 - 1`; calls must
    /// bound that argument or the recursion depth blows the native stack.
    recursive: bool,
}

struct Gen {
    rng: XorShift64,
    out: String,
    /// Bound variables currently in scope, by type.
    ints: Vec<String>,
    floats: Vec<String>,
    bools: Vec<String>,
    strs: Vec<String>,
    /// Integer arrays in scope with their (exact) literal lengths, so
    /// indexing can be guarded to stay in bounds.
    int_arrays: Vec<(String, usize)>,
    /// Callable generated functions.
    fns: Vec<FnSig>,
    /// Name uniquifier.
    fresh: usize,
}

/// Int literals weighted toward semantics-stressing values: the 48-bit
/// payload boundary (checked arithmetic must reject/wrap identically in
/// every backend), small values, and random 32-bit-ish magnitudes.
const INT_SPECIALS: &[i64] = &[
    0,
    1,
    -1,
    2,
    -2,
    7,
    42,
    255,
    4096,
    140_737_488_355_327,  // 2^47 - 1 (max 48-bit signed)
    140_737_488_355_328,  // 2^47 (overflow boundary)
    -140_737_488_355_328, // -(2^47)
    -140_737_488_355_327,
    1_000_000_000,
    -999_999_999,
    2_147_483_647,
    -2_147_483_648,
];

/// Float literals: signed zero (NaN-canonicalization and sign-bit handling
/// must agree across backends), common decimals, and large magnitudes.
const FLOAT_SPECIALS: &[&str] = &[
    "0.0",
    "-0.0",
    "1.0",
    "-1.0",
    "0.5",
    "1.5",
    "-2.5",
    "3.14",
    "0.1",
    "0.2",
    "2.0",
    "10.0",
    "123456.789",
    "-98765.4321",
    "1000000000.5",
];

const STR_SPECIALS: &[&str] = &[
    "\"\"",
    "\"a\"",
    "\"b\"",
    "\"hello\"",
    "\"xyz\"",
    "\"0\"",
    "\"-1\"",
    "\"foo bar\"",
    "\"aBc\"",
    "\"  \"",
];

impl Gen {
    fn new(seed: u64) -> Self {
        Gen {
            // Mix the seed so adjacent seeds diverge (a plain `seed | C`
            // ORs away the low bit — seeds 0 and 1 would collide), and
            // avoid the all-zero xorshift sink state.
            rng: XorShift64(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A_D3C3_B4A5),
            out: String::new(),
            ints: Vec::new(),
            floats: Vec::new(),
            bools: Vec::new(),
            strs: Vec::new(),
            int_arrays: Vec::new(),
            fns: Vec::new(),
            fresh: 0,
        }
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        self.rng.range(lo, hi)
    }

    fn chance(&mut self, pct: usize) -> bool {
        self.range(0, 100) < pct
    }

    fn fresh_name(&mut self, prefix: &str) -> String {
        let n = self.fresh;
        self.fresh += 1;
        format!("{}{}", prefix, n)
    }

    fn vars_of(&self, ty: Ty) -> &Vec<String> {
        match ty {
            Ty::Int => &self.ints,
            Ty::Float => &self.floats,
            Ty::Bool => &self.bools,
            Ty::Str => &self.strs,
        }
    }

    fn push_var(&mut self, ty: Ty, name: String) {
        match ty {
            Ty::Int => self.ints.push(name),
            Ty::Float => self.floats.push(name),
            Ty::Bool => self.bools.push(name),
            Ty::Str => self.strs.push(name),
        }
    }

    // --- leaf literals ---------------------------------------------------

    fn int_lit(&mut self) -> String {
        if self.chance(55) {
            INT_SPECIALS[self.range(0, INT_SPECIALS.len())].to_string()
        } else {
            // Random medium-magnitude int (negative half the time).
            let v = self.rng.next() % 1_000_003;
            if self.chance(50) {
                format!("{}", v as i64)
            } else {
                format!("-{}", v as i64)
            }
        }
    }

    fn float_lit(&mut self) -> String {
        if self.chance(60) {
            FLOAT_SPECIALS[self.range(0, FLOAT_SPECIALS.len())].to_string()
        } else {
            let a = self.rng.next() % 10_000;
            let b = self.rng.next() % 1000;
            let s = format!("{}.{:03}", a, b);
            if self.chance(50) {
                s
            } else {
                format!("-{}", s)
            }
        }
    }

    fn str_lit(&mut self) -> String {
        STR_SPECIALS[self.range(0, STR_SPECIALS.len())].to_string()
    }

    fn bool_lit(&mut self) -> String {
        if self.chance(50) {
            "true".to_string()
        } else {
            "false".to_string()
        }
    }

    // --- nonzero divisor guard -------------------------------------------
    //
    // Div/mod by zero yields nil in Nulang, which poisons any surrounding
    // arithmetic with a type error. That is a valid outcome to compare
    // across backends, but only when it happens identically everywhere;
    // to keep most programs error-free we make divisors provably nonzero:
    // `(e) * 2 + 1` is always odd, hence never zero, under any wrapping or
    // checked semantics that still yields a value. Occasionally we emit an
    // explicit `if d == 0 then 1 else d` guard to exercise the comparison
    // path too.

    fn nonzero_int(&mut self, depth: usize) -> String {
        if self.chance(30) {
            let d = self.expr(Ty::Int, depth.saturating_sub(1));
            format!("(if {} == 0 then 1 else ({}))", strip_parens(&d), d)
        } else {
            let d = self.expr(Ty::Int, depth.saturating_sub(1));
            format!("(({}) * 2 + 1)", d)
        }
    }

    fn nonzero_float(&mut self, depth: usize) -> String {
        if self.chance(30) {
            let d = self.expr(Ty::Float, depth.saturating_sub(1));
            format!("(if {} == 0.0 then 1.0 else ({}))", strip_parens(&d), d)
        } else {
            // |x| + 1.0 is always >= 1.0. Nulang has no abs in the grammar
            // subset we target, so square and add: e*e >= 0. The whole
            // divisor must be parenthesized: bare `((d)*(d)) + 1.0` under
            // `/` parses as `(a / (d*d)) + 1.0`, which divides by zero
            // when d == 0.0.
            let d = self.expr(Ty::Float, depth.saturating_sub(1));
            format!("((({}) * ({})) + 1.0)", d, d)
        }
    }

    /// Nulang rejects parentheses around `if`/`while` conditions; generated
    /// bool expressions are parenthesized for safety elsewhere, so strip one
    /// outer pair when emitting a condition position.
    fn cond(&mut self, depth: usize) -> String {
        let c = self.expr(Ty::Bool, depth);
        // The parser rejects a condition that *starts* with '(' at all, so
        // stripping one outer pair is not enough when the inner expression
        // is itself parenthesized (e.g. `((a) < (b))` -> `(a) < (b)`).
        // Double-negation wraps the original parenthesized form under
        // `not`, which is accepted, and is semantically transparent.
        if c.starts_with('(') && c.ends_with(')') && strip_parens(&c).starts_with('(') {
            format!("not (not ({}))", c)
        } else if c.starts_with('(') && c.ends_with(')') {
            strip_parens(&c).to_string()
        } else {
            c
        }
    }

    // --- expressions ------------------------------------------------------

    fn expr(&mut self, ty: Ty, depth: usize) -> String {
        // Leaves.
        if depth == 0 || self.chance(18) {
            let vars = self.vars_of(ty).clone();
            if !vars.is_empty() && self.chance(60) {
                return vars[self.range(0, vars.len())].clone();
            }
            return match ty {
                Ty::Int => self.int_lit(),
                Ty::Float => self.float_lit(),
                Ty::Bool => self.bool_lit(),
                Ty::Str => self.str_lit(),
            };
        }

        let d = depth - 1;
        match ty {
            Ty::Int => self.int_expr(d),
            Ty::Float => self.float_expr(d),
            Ty::Bool => self.bool_expr(d),
            Ty::Str => self.str_expr(d),
        }
    }

    fn maybe_call(&mut self, ty: Ty, depth: usize) -> Option<String> {
        let candidates: Vec<usize> = self
            .fns
            .iter()
            .enumerate()
            .filter(|(_, f)| f.ret == ty && f.params.len() <= 2)
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() || !self.chance(25) {
            return None;
        }
        let fi = candidates[self.range(0, candidates.len())];
        let (name, params, recursive) = {
            let f = &self.fns[fi];
            (f.name.clone(), f.params.clone(), f.recursive)
        };
        let mut args = Vec::new();
        for (k, p) in params.iter().enumerate() {
            let arg = self.expr(*p, depth.saturating_sub(1));
            // Recursive fns count down from their first Int argument;
            // bound it to single/double digits so the call chain stays
            // shallow regardless of where the call is generated.
            if k == 0 && recursive && *p == Ty::Int {
                args.push(format!("(({}) % 89)", arg));
            } else {
                args.push(arg);
            }
        }
        Some(format!("{}({})", name, args.join(", ")))
    }

    fn int_expr(&mut self, depth: usize) -> String {
        if let Some(call) = self.maybe_call(Ty::Int, depth) {
            return call;
        }
        match self.range(0, 14) {
            // + - *
            0 | 1 | 2 | 3 => {
                let op = ["+", "-", "*"][self.range(0, 3)];
                format!(
                    "({} {} {})",
                    self.expr(Ty::Int, depth),
                    op,
                    self.expr(Ty::Int, depth)
                )
            }
            // / % with nonzero divisor
            4 | 5 => {
                let op = ["%", "/"][self.range(0, 2)];
                format!(
                    "(({}) {} {})",
                    self.expr(Ty::Int, depth),
                    op,
                    self.nonzero_int(depth)
                )
            }
            // ** with a tiny exponent to bound growth
            6 => {
                let e = self.range(0, 5);
                format!("(({}) ** {})", self.expr(Ty::Int, depth), e)
            }
            // unary neg
            7 => format!("(-({}))", self.expr(Ty::Int, depth)),
            // if
            8 => format!(
                "(if {} then {} else {})",
                self.cond(depth),
                self.expr(Ty::Int, depth),
                self.expr(Ty::Int, depth)
            ),
            // match on a small int
            9 => format!(
                "(match ({}) {{ 0 => {}, 1 => {}, _ => {} }})",
                self.expr(Ty::Int, depth),
                self.expr(Ty::Int, depth),
                self.expr(Ty::Int, depth),
                self.expr(Ty::Int, depth)
            ),
            // array length / guarded index
            10 | 11 => {
                if self.int_arrays.is_empty() {
                    return self.expr(Ty::Int, depth);
                }
                let ai = self.range(0, self.int_arrays.len());
                let (name, len) = self.int_arrays[ai].clone();
                if self.chance(40) {
                    // Array length as a generator-tracked literal: the
                    // stdlib-less pipeline has no `Array.length` binding.
                    format!("{}", len)
                } else {
                    // Index guarded to [0, len): i >= 0 via i*i, then % len.
                    let idx = self.expr(Ty::Int, depth);
                    format!(
                        "(if 0 <= (({idx}) * ({idx})) % {len} then {name}[(({idx}) * ({idx})) % {len}] else 0)",
                        idx = idx,
                        len = len,
                        name = name
                    )
                }
            }
            // record field (int)
            12 => {
                let a = self.expr(Ty::Int, depth);
                let b = self.expr(Ty::Int, depth);
                format!(
                    "({{x: {}, y: {}}}.{})",
                    a,
                    b,
                    if self.chance(50) { "x" } else { "y" }
                )
            }
            // variable or literal fallback
            _ => {
                let vars = self.ints.clone();
                if !vars.is_empty() && self.chance(70) {
                    vars[self.range(0, vars.len())].clone()
                } else {
                    self.int_lit()
                }
            }
        }
    }

    fn float_expr(&mut self, depth: usize) -> String {
        if let Some(call) = self.maybe_call(Ty::Float, depth) {
            return call;
        }
        match self.range(0, 10) {
            0 | 1 | 2 => {
                let op = ["+", "-", "*"][self.range(0, 3)];
                format!(
                    "({} {} {})",
                    self.expr(Ty::Float, depth),
                    op,
                    self.expr(Ty::Float, depth)
                )
            }
            // float div with nonzero divisor (0.0-div yields nil)
            3 => format!(
                "(({}) / {})",
                self.expr(Ty::Float, depth),
                self.nonzero_float(depth)
            ),
            // pow with small exponent (float pow was a past AOT bug class)
            4 => {
                let e = self.range(0, 4);
                format!("(({}) ** {}.0)", self.expr(Ty::Float, depth), e)
            }
            5 => format!("(-({}))", self.expr(Ty::Float, depth)),
            6 => format!(
                "(if {} then {} else {})",
                self.cond(depth),
                self.expr(Ty::Float, depth),
                self.expr(Ty::Float, depth)
            ),
            _ => {
                let vars = self.floats.clone();
                if !vars.is_empty() && self.chance(70) {
                    vars[self.range(0, vars.len())].clone()
                } else {
                    self.float_lit()
                }
            }
        }
    }

    fn bool_expr(&mut self, depth: usize) -> String {
        match self.range(0, 10) {
            // int comparison
            0 | 1 | 2 => {
                let op = ["<", "<=", ">", ">=", "==", "!="][self.range(0, 6)];
                format!(
                    "(({}) {} ({}))",
                    self.expr(Ty::Int, depth),
                    op,
                    self.expr(Ty::Int, depth)
                )
            }
            // float comparison
            3 => {
                let op = ["<", "<=", ">", ">=", "==", "!="][self.range(0, 6)];
                format!(
                    "(({}) {} ({}))",
                    self.expr(Ty::Float, depth),
                    op,
                    self.expr(Ty::Float, depth)
                )
            }
            // string comparison
            4 => {
                let op = if self.chance(70) { "==" } else { "!=" };
                format!(
                    "(({}) {} ({}))",
                    self.expr(Ty::Str, depth),
                    op,
                    self.expr(Ty::Str, depth)
                )
            }
            5 | 6 => {
                let op = if self.chance(50) { "and" } else { "or" };
                format!(
                    "(({}) {} ({}))",
                    self.expr(Ty::Bool, depth),
                    op,
                    self.expr(Ty::Bool, depth)
                )
            }
            7 => format!("(not ({}))", self.expr(Ty::Bool, depth)),
            8 => format!(
                "(if {} then {} else {})",
                self.cond(depth),
                self.expr(Ty::Bool, depth),
                self.expr(Ty::Bool, depth)
            ),
            _ => {
                let vars = self.bools.clone();
                if !vars.is_empty() && self.chance(70) {
                    vars[self.range(0, vars.len())].clone()
                } else {
                    self.bool_lit()
                }
            }
        }
    }

    fn str_expr(&mut self, depth: usize) -> String {
        if let Some(call) = self.maybe_call(Ty::Str, depth) {
            return call;
        }
        match self.range(0, 8) {
            // concat
            0 | 1 | 2 => format!(
                "({} + {})",
                self.expr(Ty::Str, depth),
                self.expr(Ty::Str, depth)
            ),
            // string + int coercion (past backend bug class)
            3 => format!(
                "({} + {})",
                self.expr(Ty::Str, depth),
                self.expr(Ty::Int, depth)
            ),
            4 => format!(
                "(if {} then {} else {})",
                self.cond(depth),
                self.expr(Ty::Str, depth),
                self.expr(Ty::Str, depth)
            ),
            _ => {
                let vars = self.strs.clone();
                if !vars.is_empty() && self.chance(70) {
                    vars[self.range(0, vars.len())].clone()
                } else {
                    self.str_lit()
                }
            }
        }
    }

    // --- statements --------------------------------------------------------
    //
    // Statements append `let`/`var` bindings, array construction, function
    // declarations, and bounded loops to `self.out`, extending the
    // environment. All loops have constant-bounded trip counts so generated
    // programs always terminate well under the VM step limit.

    fn stmt(&mut self, depth: usize) {
        match self.range(0, 12) {
            // let binding of a random type
            0 | 1 | 2 | 3 | 4 => {
                let ty = [Ty::Int, Ty::Int, Ty::Float, Ty::Bool, Ty::Str][self.range(0, 5)];
                let name = self.fresh_name(match ty {
                    Ty::Int => "i",
                    Ty::Float => "f",
                    Ty::Bool => "b",
                    Ty::Str => "s",
                });
                let val = self.expr(ty, depth);
                self.out.push_str(&format!("let {} = {}\n", name, val));
                self.push_var(ty, name);
            }
            // int array literal
            5 | 6 => {
                let len = self.range(1, 6);
                let elems: Vec<String> = (0..len)
                    .map(|_| self.expr(Ty::Int, depth.saturating_sub(1)))
                    .collect();
                let name = self.fresh_name("a");
                self.out
                    .push_str(&format!("let {} = [{}]\n", name, elems.join(", ")));
                self.int_arrays.push((name, len));
            }
            // top-level fn decl (pure expression body)
            7 | 8 => {
                let ret = [Ty::Int, Ty::Int, Ty::Float, Ty::Bool, Ty::Str][self.range(0, 5)];
                let nparams = self.range(1, 3);
                let ptypes: Vec<Ty> = (0..nparams)
                    .map(|_| [Ty::Int, Ty::Int, Ty::Float, Ty::Bool][self.range(0, 4)])
                    .collect();
                let name = self.fresh_name("g");
                let saved = (
                    self.ints.clone(),
                    self.floats.clone(),
                    self.bools.clone(),
                    self.strs.clone(),
                    self.int_arrays.clone(),
                );
                // Top-level `fn` bodies cannot capture top-level `let`
                // bindings (the typechecker reports them as unbound), so
                // the body is generated against the params alone.
                self.ints.clear();
                self.floats.clear();
                self.bools.clear();
                self.strs.clear();
                self.int_arrays.clear();
                let pnames: Vec<String> = ptypes
                    .iter()
                    .enumerate()
                    .map(|(k, &t)| {
                        let pn = format!("p{}", k);
                        self.push_var(t, pn.clone());
                        pn
                    })
                    .collect();
                // Recursive body: the function name is in scope inside its
                // own body. Bound recursion by guarding on the first int
                // param when present; otherwise keep the body shallow.
                let recursive = ptypes.first() == Some(&Ty::Int)
                    && matches!(ret, Ty::Int | Ty::Float)
                    && self.chance(30);
                let body = if recursive {
                    let rec_args: Vec<String> = ptypes
                        .iter()
                        .enumerate()
                        .map(|(k, &t)| {
                            if k == 0 {
                                format!("({} - 1)", pnames[0])
                            } else {
                                self.expr(t, 0)
                            }
                        })
                        .collect();
                    format!(
                        "if {} <= 0 then {} else ({} + {}({}))",
                        pnames[0],
                        self.expr(ret, 1),
                        self.expr(ret, 1),
                        name,
                        rec_args.join(", ")
                    )
                } else {
                    self.expr(ret, depth)
                };
                self.out.push_str(&format!(
                    "fn {}({}) {{ {} }}\n",
                    name,
                    pnames.join(", "),
                    body
                ));
                let (i, f, b, s, ia) = saved;
                self.ints = i;
                self.floats = f;
                self.bools = b;
                self.strs = s;
                self.int_arrays = ia;
                self.fns.push(FnSig {
                    name,
                    params: ptypes,
                    ret,
                    recursive,
                });
            }
            // bounded while loop over var accumulators (forces hot regions
            // when the trip count crosses the JIT threshold)
            9 => {
                let trips = if self.chance(15) {
                    // Hot loop: crosses HOT_THRESHOLD inside a single run.
                    self.range(1050, 1600)
                } else {
                    self.range(1, 40)
                };
                let acc = self.fresh_name("acc");
                let ivar = self.fresh_name("k");
                let step = self.expr(Ty::Int, 1);
                let res = self.fresh_name("i");
                self.out.push_str(&format!(
                    "let {res} = {{ var {acc} = 0\nvar {ivar} = 0\nwhile {ivar} < {trips} {{ {acc} = {acc} + ({step})\n{ivar} = {ivar} + 1 }}\n{acc} }}\n",
                    res = res,
                    acc = acc,
                    ivar = ivar,
                    trips = trips,
                    step = step,
                ));
                self.ints.push(res);
            }
            // for-in loop over a small literal array
            10 => {
                let len = self.range(1, 5);
                let elems: Vec<String> = (0..len).map(|_| self.expr(Ty::Int, 1)).collect();
                let acc = self.fresh_name("fa");
                let x = self.fresh_name("x");
                let res = self.fresh_name("i");
                self.out.push_str(&format!(
                    "let {res} = {{ var {acc} = 0\nfor {x} in [{elems}] {{ {acc} = {acc} + {x} }}\n{acc} }}\n",
                    res = res,
                    acc = acc,
                    x = x,
                    elems = elems.join(", ")
                ));
                self.ints.push(res);
            }
            // record binding with field access and functional update
            _ => {
                let rname = self.fresh_name("r");
                let fx = self.expr(Ty::Int, depth.saturating_sub(1));
                let fy = self.expr(Ty::Int, depth.saturating_sub(1));
                self.out
                    .push_str(&format!("let {} = {{x: {}, y: {}}}\n", rname, fx, fy));
                let vname = self.fresh_name("i");
                let upd = self.expr(Ty::Int, depth.saturating_sub(1));
                let field = if self.chance(50) { "x" } else { "y" };
                self.out.push_str(&format!(
                    "let {} = ({{{} .. {} = {}}}).{}\n",
                    vname, rname, field, upd, field
                ));
                self.ints.push(vname);
            }
        }
    }
}

/// Strip one outer paren pair, if present (generated sub-expressions are
/// defensively parenthesized; some positions forbid them).
fn strip_parens(s: &str) -> &str {
    if s.starts_with('(') && s.ends_with(')') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Generate a complete, well-formed Nulang program deterministically from
/// `seed`. The final expression has a safely cross-backend-comparable type
/// (Int/Float/Bool/String — no closures, actor refs, or heap objects whose
/// identity is run-dependent).
pub fn generate_program(seed: u64) -> String {
    let mut g = Gen::new(seed);
    let nstmts = g.range(1, 9);

    // Occasionally wrap the whole program in an effect perform/handle
    // exercise (bounded): a Tick effect whose handler resumes with x+1,
    // accumulated in a while loop.
    let effect_wrapper = g.chance(12);

    for _ in 0..nstmts {
        g.stmt(2);
    }

    // Final expression: a safely comparable type. Bind it to a name and
    // return the bare identifier: a parenthesized expression on the line
    // after a statement is parsed as a *call* of the previous line's
    // trailing expression (juxtaposition application), which would both
    // mis-parse and leave bindings out of scope.
    let final_ty = [Ty::Int, Ty::Int, Ty::Int, Ty::Float, Ty::Bool, Ty::Str][g.range(0, 6)];
    let final_expr = format!("let __final = {}\n__final", g.expr(final_ty, 3));

    if effect_wrapper {
        // handler resumes x+1; loop sums resumed values for i in 0..n
        let n = g.range(1, 30);
        format!(
            "{}\neffect Tick {{ next: Int -> Int }}\nfn __drive(n: Int) -> Int {{\n    var acc = 0\n    var i = 0\n    handle {{\n        while i < n {{\n            acc = acc + perform Tick.next(i)\n            i = i + 1\n        }}\n    }} {{\n        | Tick.next(x) resume => resume(x + 1)\n    }}\n    acc\n}}\nlet __effect_sum = __drive({})\n{}",
            g.out, n, final_expr
        )
    } else {
        format!("{}\n{}", g.out, final_expr)
    }
}

// ---------------------------------------------------------------------------
// Campaign runner
// ---------------------------------------------------------------------------

/// One differential divergence: the seed, the generated source, and the
/// oracle's description of which backends disagreed and how.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub seed: u64,
    pub source: String,
    pub message: String,
}

/// Aggregate statistics for a campaign run.
#[derive(Debug, Default)]
pub struct CampaignStats {
    pub generated: usize,
    /// Compiled to bytecode and all compared backends agreed.
    pub agreed: usize,
    /// Subset of `agreed` where AOT also compiled and agreed.
    pub aot_agreed: usize,
    /// Result type not comparable across independent runs (closure/actor).
    pub uncomparable: usize,
    /// Generated program failed to compile — a generator or frontend bug
    /// worth reporting (kept as sources for inspection).
    pub compile_failures: Vec<(u64, String)>,
    /// Divergences in the known 48-bit-overflow semantics gap: the
    /// interpreter raises a checked-overflow error while the JIT/AOT
    /// backends wrap (AOT pins this in
    /// `aot::codegen::tests::test_aot_float_pow_and_int_pow_overflow`),
    /// record-and-continue with nil, or raise at a different op. Tracked
    /// and persisted separately from unknown divergences so campaigns
    /// stay signal-rich; see docs/DIFFERENTIAL_FUZZING.md "Findings".
    pub known_overflow: Vec<Divergence>,
    pub divergences: Vec<Divergence>,
}

/// A divergence belongs to the known overflow-semantics class iff the
/// checked 48-bit overflow error appears on at least one side of the
/// disagreement (the oracle message embeds each backend's outcome).
pub fn is_overflow_semantics_divergence(message: &str) -> bool {
    message.contains("exceeds the 48-bit range")
}

/// Run a differential campaign over seeds `base_seed .. base_seed + count`
/// (stopping early at `deadline` when given). On divergence the source is
/// persisted to `crasher_dir/<seed>.nula` with the oracle message in a
/// header comment, and the seed is printed on stdout. Divergences in the
/// known 48-bit-overflow semantics class (`is_overflow_semantics_divergence`)
/// are persisted under `crasher_dir/known-overflow/` and tallied apart from
/// untriaged divergences.
pub fn run_campaign(
    base_seed: u64,
    count: u64,
    deadline: Option<std::time::Instant>,
    crasher_dir: Option<&std::path::Path>,
    verbose: bool,
) -> CampaignStats {
    let mut stats = CampaignStats::default();
    if let Some(dir) = crasher_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    for i in 0..count {
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                break;
            }
        }
        let seed = base_seed.wrapping_add(i);
        let source = generate_program(seed);
        stats.generated += 1;

        // Generated programs are supposed to be well-formed; a compile
        // failure means the generator escaped the grammar (or the frontend
        // rejects a legal program). Track separately from divergences.
        if crate::fuzz::compile_for_diff(&source).is_none() {
            stats.compile_failures.push((seed, source));
            continue;
        }

        match differential_fuzz_one(&source) {
            Ok(DiffOutcome::NothingToCompile) => {
                stats.compile_failures.push((seed, source));
            }
            Ok(DiffOutcome::Uncomparable) => stats.uncomparable += 1,
            Ok(DiffOutcome::Agreed { aot, .. }) => {
                stats.agreed += 1;
                if aot {
                    stats.aot_agreed += 1;
                }
            }
            Err(message) => {
                let known = is_overflow_semantics_divergence(&message);
                if verbose {
                    eprintln!(
                        "{} seed={:#x}: {}",
                        if known {
                            "KNOWN-OVERFLOW"
                        } else {
                            "DIVERGENCE"
                        },
                        seed,
                        message
                    );
                }
                if let Some(dir) = crasher_dir {
                    // Known-class crashers go to a subdirectory so the top
                    // level only ever holds untriaged divergences.
                    let dir = if known {
                        dir.join("known-overflow")
                    } else {
                        dir.to_path_buf()
                    };
                    let _ = std::fs::create_dir_all(&dir);
                    let path = dir.join(format!("seed_{:016x}.nula", seed));
                    let body = format!(
                        "// Differential fuzzer crasher\n// seed: {0} (0x{0:x})\n// {1}\n// reproduce: nula_difffuzz --seeds 1 --seed-base {0}\n\n{2}\n",
                        seed,
                        message.replace('\n', "\n// "),
                        source
                    );
                    let _ = std::fs::write(path, body);
                }
                let d = Divergence {
                    seed,
                    source,
                    message,
                };
                if known {
                    stats.known_overflow.push(d);
                } else {
                    stats.divergences.push(d);
                }
            }
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator must be deterministic: same seed, same program.
    #[test]
    fn generator_is_deterministic() {
        for seed in [0u64, 1, 42, 0xDEAD_BEEF, u64::MAX] {
            assert_eq!(generate_program(seed), generate_program(seed));
        }
    }

    /// Every generated program in a fixed sample must compile through the
    /// full frontend (lex -> parse -> typecheck -> HIR -> MIR -> bytecode).
    /// A failure here is a generator bug (it escaped the grammar), not a
    /// backend divergence — keep the generator honest.
    #[test]
    fn generated_programs_compile() {
        let mut failures = Vec::new();
        for seed in 0..200u64 {
            let src = generate_program(seed);
            if let Err(e) = crate::fuzz::compile_for_diff_verbose(&src) {
                failures.push((seed, e, src));
            }
        }
        if !failures.is_empty() {
            for (seed, err, src) in &failures {
                eprintln!("COMPILE FAILURE seed={} [{}]:\n{}\n---", seed, err, src);
            }
            panic!(
                "{} of 200 generated programs failed to compile",
                failures.len()
            );
        }
    }

    /// CI smoke test: 50 fixed seeds, interpreter vs forced-JIT vs AOT all
    /// agree. Runs as part of the default `cargo test` suite (both debug and
    /// release); the heavier campaigns run via scripts/difffuzz.sh.
    #[test]
    fn differential_smoke_50_seeds() {
        let stats = run_campaign(0xD1FF_0000, 50, None, None, false);
        eprintln!(
            "difffuzz smoke: {} generated, {} agreed ({} with AOT), {} uncomparable, {} compile failures, {} known-overflow, {} divergences",
            stats.generated,
            stats.agreed,
            stats.aot_agreed,
            stats.uncomparable,
            stats.compile_failures.len(),
            stats.known_overflow.len(),
            stats.divergences.len()
        );
        assert!(
            stats.compile_failures.is_empty(),
            "generator produced uncompilable programs: {:?}",
            stats
                .compile_failures
                .iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
        );
        assert!(
            stats.divergences.is_empty(),
            "differential divergences: {:?}",
            stats
                .divergences
                .iter()
                .map(|d| (&d.seed, &d.message))
                .collect::<Vec<_>>()
        );
        // Sanity: the smoke run must actually compare something.
        assert!(stats.agreed > 25, "suspiciously low agreement count");
    }
}

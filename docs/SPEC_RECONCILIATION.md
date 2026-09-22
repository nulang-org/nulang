# Spec Reconciliation Audit Trail

> **Historical audit note:** This file records an earlier reconciliation pass
> and intentionally preserves the findings that were true at that time. It is
> not a live implementation-status dashboard. In particular, the CRDT finding
> in item 7 was superseded by later source-level `state crdt <type>` and
> `Crdt.*` implementations/conformance coverage. For current status use
> `SPEC2.md`, `CHANGELOG.md`, `GOVERNANCE.md`, and
> `docs/SEMANTIC_STABILIZATION_CONTRACT.md`.

Reconciliation pass over the normative documents (`SPEC2.md`,
`spec/grammar.ebnf`, `docs/PITFALLS.md`, `README.md`) against the
reference implementation. Ground truth for every item below was
established by reading `src/parser.rs` (the reference parser per
`GOVERNANCE.md`/`spec/grammar.ebnf` §5), `src/lexer.rs`,
`src/typechecker.rs`, `src/effect_checker.rs`, `src/hir_lower.rs`, and
`src/vm.rs`. No Rust source and no `.nula` program logic was modified.

Branch: `fix/spec-reconciliation`.

## 1. `spec/grammar.ebnf` duplicate productions — CONFIRMED, fixed

- **Discrepancy:** `fn_decl` was defined twice with different
  return-type sigils (`-> type` and `: type`); `send_expr` was defined
  twice (`send expr to expr` and `send expr IDENT(...)`).
- **Ground truth:**
  - Return-type sigil is `->` (`TokenKind::Arrow`):
    `src/parser.rs:654` (`parse_function`).
  - Keyword send is `send [remote] actor behavior(args)` with mandatory
    parentheses: `src/parser.rs:3721-3735` (`parse_send_keyword`).
    There is no `send expr to expr` production anywhere in the parser.
  - Postfix send `actor ! behavior(args)` and async tell
    `actor <- behavior(args)` are Pratt postfix operators:
    `src/parser.rs:2466-2479`, `2483-2495`; both desugar to the same
    `Expr::Send`.
- **Change:** grammar now has one `fn_decl` production (with `->`) and
  one `send_expr` production (keyword form), with the postfix forms
  documented in a comment. `handle_expr` was also corrected (item 2).

## 2. `handle ... with` — CONFIRMED, fixed

- **Discrepancy:** SPEC2 §4.4 said "there is no `with` keyword";
  `examples/07_effects.nula` uses `handle ... with { ... }` throughout.
- **Ground truth:** `with` is optional after the handled body
  (`src/parser.rs:4182`, `consume_if(&TokenKind::With)`), and
  `handle body with name` references a prior `handler name { ... }`
  declaration (`src/parser.rs:4185-4195`, `2044-2085`).
- **Change:** SPEC2 §4.4 and `spec/grammar.ebnf` `handle_expr` now
  document the optional `with` (bare block, `with { ... }`, and
  `with name` forms).

## 3. Implemented-vs-Planned annotations — CONFIRMED, fixed

| Claim (old SPEC2) | Ground truth | Source |
|---|---|---|
| §2.6 "no `**` exponentiation operator" | `**` is implemented, right-assoc, above `*` | `src/lexer.rs:269` (`Star2`); `src/parser.rs:45,93` (`PREC_EXP` = 14) |
| §2.3 "no `var` keyword" | `var` is a keyword; mutable bindings parse | `src/lexer.rs:1185`; `src/parser.rs:2242`, `2951` |
| §2.3 "no `consume`/`recover`/`as` keyword" | all three are keywords | `src/lexer.rs:1266`, `1267`, `1197`; `src/parser.rs:3034` |
| §2.7 "`..`, `::`, `<-`, `?` not accepted anywhere" | all four parse: ranges, record update, import paths, async tell, try `?`, optional chaining `?.` | `src/parser.rs:83/5698`, `2974-3000`, `2208-2213`, `2483`, `2603`, `2567` |
| §3.5 record update `{ r .. f = v }` Planned | implemented | `src/ast.rs:152` (`Expr::RecordUpdate`), `src/parser.rs:2997` |
| §2.6.4 `consume`/`recover` rows "Planned" | both implemented (Experimental) | as above |

`enum`, `event`, `from`, `config`, `capability` remain non-keywords
(absent from the `src/lexer.rs` keyword table); §2.3 now says exactly
that. Also corrected the stale `PREC_EXP` level (13 → 14,
`src/parser.rs:45`) in the status section and added `**` to the §2.6.1
arithmetic table and the range-operator cross-reference in §2.5.

## 4. `recover` — CONFIRMED, resolved

- **Discrepancy:** `docs/PITFALLS.md` (and the SPEC2 status section)
  said `recover { body }` wraps the result in `Ok`/`Error`; SPEC2
  §3.9.2/§6.13 said `recover` is not a keyword and reserved it for
  Pony-style capability recovery.
- **Ground truth:** `recover` IS a keyword (`src/lexer.rs:1267`) and
  parses as `Expr::Recover` (`src/parser.rs:4626-4632`). The
  typechecker infers the body's type unchanged
  (`src/typechecker.rs:1814`) — there is no `Ok`/`Error` wrapping
  anywhere in the pipeline — and HIR lowering is transparent
  (`src/hir_lower.rs:2062`). The single non-transparent check is in the
  capability checker: the body's result must be sendable, else
  "recover body must evaluate to a sendable value"
  (`src/effect_checker.rs:2023-2033`).
- **Resolution:** the single current meaning — an isolated scope with a
  sendable-result check, no wrapping, no capability upgrade — is
  documented in SPEC2 §3.9.2/§6.13 and PITFALLS. The Pony-style
  capability-upgrade semantics (constructing `iso`/`val` from
  restricted interiors) is explicitly marked reserved/future.

## 5. Effect set (`Net`/`Rand`) — CONFIRMED, fixed

- **Discrepancy:** Appendix C's intro listed `Net` and `Rand` among
  the recognized built-ins while §4.6 (corrected 2026-08-02) says those
  names have no runtime dispatch.
- **Ground truth:** `parse_effect_name` (`src/effect_checker.rs:73-97`)
  recognizes `IO`, `Net`, `Http`, `FS`, `Array`, `String`, `Test`,
  `Rand`, `Random`, `Time`, `Spawn`, `Send`, `Receive`, `Migrate`,
  `STM`, `Async`, `Inference`, `Cost`, `Event`, `FFI`, `DB`, `Python`,
  `Process`, `System`; `Net`→`Effect::Net` and `Rand`→`Effect::Rand`
  are compile-time aliases. The VM dispatches on the literal names
  `Http` (`src/vm.rs:926`) and `Random` (`src/vm.rs:1075`); there is no
  `Net`/`Rand` dispatch, so `perform Net.get(...)` fails with
  "Unhandled effect". `LLM` is handled as an `Inference` alias in
  `src/cir_lower.rs:58`.
- **Change:** Appendix C now lists the full recognized set, explains
  the recognition-vs-dispatch distinction, and directs users to
  `Http`/`Random`. §2.3's naming-convention example updated likewise.

## 6. Known design tensions — documented (no behavior change)

Added **SPEC2 Appendix E: Known Design Tensions Under Review for v2**,
listing with source citations:

1. `!` = prefix not / send `a!b` / effect row `!{IO}` / error type `!E`
   (`src/parser.rs` `prefix_precedence`, `:2466`, `:663`).
2. `&` = prefix borrow / bitwise AND (`src/parser.rs:42`) / the `&&`
   family.
3. Bitwise binds tighter than arithmetic: `1 + 2 & 3` = `1 + (2 & 3)`
   (Pratt table `src/parser.rs:30-46`; quirk already noted in §2.6).
4. Division by zero yields `nil` (`src/vm.rs` `step_idiv`,
   `:3655-3672`; §2.6.1), bypassing the `?`/error-value surface.

Each carries the recommendation that it be resolved by RFC before the
next frozen-tier bump.

## 7. CRDTs and multi-node distribution — CONFIRMED, fixed

- **Discrepancy:** README's feature list implied the 8 CRDT types ship
  with distribution, and its stability table listed "CRDT operations"
  as **Stable**; SPEC2's intro said "CRDT state converges
  automatically" unqualified. In fact `state crdt` behaves as `durable`
  (SPEC2 §9.10 — already accurately documented there) and `migrate` is
  a documented no-op (§12.4); the CRDT types are exercised only by
  Rust-level tests (`src/runtime/crdt.rs`, `CrdtManager`), with no
  `.nula`-level surface (§12.5).
- **Change:** README scopes CRDTs to the Rust embedder level and moves
  them from the Stable row to the Experimental row; SPEC2's overview
  sentences now carry explicit Planned/Experimental markers pointing at
  §9.10/§12.5. (§9.10, §12.4, §12.5 themselves were already accurate
  and were not changed.)

## Discrepancies checked and NOT found

- **`send expr to expr`** — not just absent from the grammar's intent;
  confirmed no such production exists in `src/parser.rs` (removed from
  the EBNF rather than "fixed to match").
- **Pony-style `recover` capability upgrade** — confirmed absent from
  typechecker, effect checker, and lowering (only the sendability check
  exists); documented as future, not current.
- **`Ok`/`Error` wrapping in `recover`** — confirmed absent
  (`src/typechecker.rs:1814` infers the body type unchanged). The
  PITFALLS/status-section claim was simply wrong and was corrected.

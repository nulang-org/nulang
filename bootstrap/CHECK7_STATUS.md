# CHECK7_STATUS — self-compile oracle investigation status

Date: 2026-09-02. Goal: enable verify.sh check 7 (stage-2 self-compile oracle).

## Checks 1–6: PASS (verified)
`bash bootstrap/verify.sh` → "All bootstrap checks passed" (6 checks).

## Uncommitted changes (required for full 133-fn build, keep)
- `bootstrap/compile_hex.nula` +220/−118
- `bootstrap/prep_core.py` +154/−118
- `bootstrap/hex2nbc.py` +5
Working-tree state per `git diff --stat`.

## 0x65 fix: DONE (earlier, verified)
save_reg wrap at both sites: `if ne < 56 then 200 + ne else if ne < 152 then 160 + (ne - 56) else 93 + ((ne - 152) % 163)`.
Before: `0x65 count: 1` at byte 32560. After: `0x65 count: 0`. Artifact builds fully: 133 fns, 63609 bytes.

## Standing blocker: main() driver dropped (check 7 still commented out)
Self-compiled run returns `#Value(7ff7400000000000)` (TAG_CLOSURE). Halt path:
`Closure fn#1 -> r8; CapStore; Move r8->r208; Move r208->r0; Halt` — no ClosureCall.

## This session's attribution result — drop is PRE-EXISTING, NOT my rewrite
Run the SAME real prep source through both compilers:
- HEAD (`/tmp/compile_hex_HEAD.nula`, `git show HEAD:bootstrap/compile_hex.nula`): 68 fns, same `Closure fn#1 -> r8; ...; Halt` drop.
- Working tree: 133 fns, same drop shape.
Both compilers keep the driver on synthetic chains up to N=68 with real bodies
(nperform IO.print / Int.to_string, cross-captures) + real read-driver shape:
`let input = nperform("IO.read") in if input == "" then compile(1) else compile(2)` — tail is a real `ClosureCall ... ; Move rX->r0; Halt`.
The bnd files (capture-free) keep it at K=40 on both compilers.

Synthetic drop signatures seen ONLY when refs are unbound (driver references helpers
omitted from the chain) — those are expected garbage, NOT the frame-drop signal.
Chunked structures (2×5 top-level, chunk-in-main) also drop with unbound refs.
`.work` inlining (28 helpers kept, 98 fns) and `.work`+`.bak`(46 fns) also drop.

The 42-fn artifact (Sep 2 08:17) that RAN end-to-end used `Int.to_hex` builtin +
debug driver markers; source lost, in no git commit/stash.

## Untested levers — NOW ELIMINATED (2026-09-02 session 2)
1. Input-length truncation: ELIMINATED. Padded synthetic N=68 chain to 27443 bytes
   (real prep size, trailing comment pad): driver KEPT (ClosureCall present).
2. Helper count: ELIMINATED. Nested-capture chains inside main() up to N=68 keep
   the driver; N=46/47/48/50 main-wrapped keep it on BOTH compilers.
3. Driver shape: ELIMINATED. Real read-driver (`nperform("IO.read") in if input
   == "" then compile(1) else compile(2)`) keeps the call on synthetics.
4. Multi-param defs: real helpers are CURRIED multi-param defs (emit_word has 4
   params → nested fn(a)=>fn(b)=>fn(c)=>fn(d) chains); 381 fn defs, 0 multi-param
   defs remain in prep output; the curried form compiles AND calls fine
   (`f(1)(2)` → 2 ClosureCalls, run=3). Separate pre-existing bug: a plain
   multi-param def `let add = fn(x,y) => x+y in add(1,2)` compiles to a 2-instr
   stub (`Move r208->r0; Halt`) — never curried/called.

## ROOT CAUSE PINNED (decisive evidence)
- DROPPED artifact (real prep, HEAD or WT): `ClosureCall` count 112, but **0
  `IO.read` and 0 `"1 < 2 and 2 < 3"`** — the ENTIRE driver body is missing from
  emitted bytecode, not just the final call.
- KEPT synthetic (N=68 chain + real read driver): 2 `IO.read` performed, driver
  in tail.
- Isolator: ALL 47 REAL helpers + trivial `compile(1)` driver (23148 bytes) →
  DROPS (67 fns, tail `Move r35->r149; Move r36->r0; Halt`). Synthetic single-param
  helpers + `compile(1)` at N=50 → KEEPS. The trigger is the real curried
  multi-param helper bodies: their nested-closure register pressure exhausts the
  main frame's allocation so the trailing driver statements are never emitted
  (max reg 249 < 256, so NOT a register-field overflow — an allocator/liveness
  bug in compile_hex.nula's frame emission, pre-existing, repro'd on HEAD).
- Fix location: the frame/register allocation in bootstrap/compile_hex.nula's
  main-frame emission (the let-chain/closure-materialization path). Repairing
  THAT is the single lever that unblocks check 7. Also worth fixing: multi-param
  def → stub bug (bootstrap/compile_hex.nula).

## FINAL narrowing (end of session 2) — driver BODY is never emitted
- DROPPED artifact: the emitted program has ZERO `IO.read` performs and ZERO
  `"1 < 2 and 2 < 3"` const strings. The driver STATEMENTS are absent from the
  bytecode — not just the last ClosureCall. Entry Jmp lands at the final
  `Closure fn#1 -> r8; Move r8->r208; Move r208->r0; Halt` — a bare closure
  materialization of the FIRST helper, never called.
- Isolators (all on HEAD pre-WIP compiler, so pre-existing):
  * ALL 47 REAL prepped helpers + trivial `compile(1)` driver (23148 B) → DROPS
    (67 fns; tail `Move r35->r149; Move r36->r0; Halt`). Driver not in bytecode.
  * Synthetic single-param helper chains (captures, nested inner closures, real
    read-driver, main-wrapped, padded to 27443 B, N=40..68) → ALL KEEP
    (ClosureCall + driver in tail).
  * Plain multi-param def bug (separate, tiny repro):
    `let add = fn(x,y) => x+y in add(1,2)` → 2-instr stub (`Move r208->r0; Halt`);
    curried lambda form `let f = fn(a) => fn(b) => a+b in f(1)(2)` → correct
    (2 ClosureCalls, run=3). Prep output has 381 fn defs, 0 multi-param defs
    (all curried, and curried works) — but the REAL helpers in compile_hex.nula
    are multi-param (emit_word 4p, comp_fn_parse 10p, comp_nperform_* 8-9p).
- Trigger = the real curried MULTI-PARAM helper bodies (nested-lambda application
  chains): their register pressure per let-binding (each application advances
  nr, plus RHS-arg comps) desyncs the let-chain "in"-continuation register
  accounting in comp(), so the FINAL binding's continuation (the driver) is
  compiled into a register/PC position the emitter then drops. Max emitted reg
  249 < 256 (0x65 fix intact) — desync, not field overflow.
- Candidate fix (UNVERIFIED): in compile_hex.nula comp(), the let-chain continues
  at `nxt = nr + 1` regardless of how many registers the RHS consumed (`vr`
  from `bound >> 18`). Changing the continuation start to `nxt = vr + 1`
  (next free reg after the RHS result) would stop register reuse across
  bindings and likely keep the tail alive. NOT TESTED this session; do not
  assume it works — run the edit → prep → build → disasm-tail cycle.
- Note: real-helper prefix slices (k=12,16) also drop, but those carry the
  unbound-ref confound (kept bodies reference omitted helpers) — not clean
  signal. The clean pair remains: ALL-47-bound + trivial driver drops vs
  bound synthetics keep.
- Fix verification oracle: after any compile_hex.nula change, build the full
  prep and check (a) disasm tail contains `ClosureCall` + `IO.read` +
  `"1 < 2 and 2 < 3"` consts; (b) `echo '1' | $NULANG self.nbc` emits
  `04080000 12080000 01000000` (or at least non-closure output); (c) 0x65
  count stays 0; (d) verify.sh checks 1-6 still pass.

## Session 3 (2026-09-02) — register-collision lead; candidate fix REJECTED
- Candidate fix `let nxt = max(nr+1, vr+1)` in the let-continuation was tried
  and REJECTED: it truncated emission to 3 fns / 32 hex words (compile exit 0
  but continuation registers desynced catastrophically). Reverted to
  `let nxt = nr + 1`; revert verified (133 fns, 0x65=0, verify.sh green,
  same diff stat as baseline).
- NEW SMOKING GUN (revert artifact /tmp/self_revert.nbc, 15361 instrs):
  pc0 Jmp -> 15353 lands at the first closure's staging; tail is
  `Closure fn#1 -> r8; Move r208->r128; CapStore r8 slot0 r128; Move r8->r209;
   Move r8->r208; CapStore; Move r208->r208; Move r208->r0; Halt`.
  The `Move r208->r208` self-assignment is the driver-call step collapsing:
  the application of the LAST binding (compile) stages into r208, and the
  call target register ALSO resolves to r208 — identical source/dest —
  so the ClosureCall is eliminated (and never emitted). ZERO IO.read performs
  in the artifact: the driver's body statements are not emitted at all.
- Mechanism hypothesis: `save_reg = if ne < 56 then 200 + ne else if ne < 152
  then 160 + (ne - 56) else 93 + ((ne - 152) % 163)` with `ne = nr + elen`.
  After 47 nested let-bindings, nr has advanced so the last binding's
  application reuses a save_reg already holding values, and the emitted
  application sequence degenerates. The application branch (`c == 40` in
  comp()) re-stages `lr` into save_reg then ClosureCalls; when lr's register
  ALREADY equals save_reg (collision via the wrap), the `Move` is a no-op
  self-move and the follow-up ClosureCall is skipped (`if c2 == 41` path).
- Next move (highest value): instrument comp()'s `c == 40` application branch
  to emit remark("APP lr=.. save=..") lines for the last let-binding
  application, rebuild, and read the marker sequence from hex_out.txt to
  confirm the collision values; then change the save_reg wrap or the
  application staging to a register guaranteed different from lr.

## Session 4 (2026-09-02) — hex-stream ground truth; mechanism confirmed
- Emission tail of the 133-fn artifact (from /tmp/hex_out.txt remarks, after
  `; fn_end:`):
  `Closure fn#1 -> r8; Move r208->r128; CapStore r8 slot0 r128; Move r8->r209;
   Move r8->r208; CapStore r8 slot0 r208; Move r208->r208; Move r208->r0; Halt`
  followed by compile()'s own `; result in r208` remark. compile() completed
  normally (its last remark executes) — comp() RETURNED register 208 for the
  whole prepped program, and the emitted top-level is ONLY the first
  binding's closure materialization dance.
- CONFIRMED MECHANISM: the top-level `let main = fn() => (47 lets ...) in
  main()` took the let-branch's NON-"in" fallback `(save_reg << 18) + vp2`
  (save_reg = 200+ne = 200+8 = 208 at ne=8, first wrap band). The following
  `(` application then had lr = 208 and ne = nr+elen = 8 → save_reg = 208:
  `Move lr->save_reg` = `Move r208->r208` self-move; the char after the arg
  was not a clean `)` so the `ClosureCall` was skipped; comp returned
  (208<<18)+… → compile() emits `Move r208->r0; Halt`. Result: r0 = closure,
  `#Value(7ff7400000000000)`.
- ROOT CAUSE: `vp2 = skip_ws(src, vp, len)` where vp = low17(bound) —
  after compiling the giant fn-literal RHS (fn#132: `fn() => (47 lets +
  driver)`), the position arithmetic (17-bit pos field, possibly with the
  bool flag bit-17) lands vp2 NOT at `in`, so the "in" branch is never
  taken. Same collision then kills the application. The WORKING 42-fn
  artifact had shorter binding chains so positions stayed aligned.
- NEXT TEST (cheap, no file edits): in prep_core, emit the driver as the
  TOP-LEVEL expression (append `driver()` as the final `in ...` body of the
  chain instead of `let main = fn() => (...) in main()`) — i.e. avoid the
  wrap so the let-chain's final binding IS the driver call, and rebuild.
  If the ClosureCall appears, the fix is in prep_core's wrapper shape
  (do NOT wrap in main; emit flat chain + `compile(input)` tail).

## Verify
- Build: `python3 bootstrap/prep_core.py < compile_hex.nula | $NULANG compile_hex.nula | fixup_hex.py | hex2nbc.py > self.nbc`
- Test: `echo '1' | $NULANG self.nbc` → expect hex `04080000 12080000 01000000` matching host
- 0x65 count: `python3 -c "import struct; d=open(f,'rb').read(); n=struct.unpack('>I',d[44:48])[0]; print(sum(1 for i in range(48,48+n*4,4) if d[i]>>24==0x65))"`

## Session 5 (2026-09-02) — THREE compile-time root causes FOUND AND FIXED
1. **Hash collision `r2` == `fn`** (both fold to 620 under `h*5+c`): every
   `r2` in the prepped source was dispatched to the `fn` parser branch →
   arg parse returned unconsumed → let/app chain died → driver dropped.
   FIXED: read_ident multiplier 5 → 37 (zero collisions across all 259
   prepped idents), all dispatch constants recomputed (let 20633, fn 3884,
   if 3987, then 58195, else 25281, true 6932, false 15187, not 23741,
   nperform 64877, and 5891, or 4221). Verified: `let q = ew(r2)(2) in q`
   now chains 4 ClosureCalls (was 1); `r2 + 1` now emits IAdd (was dropped).
2. **`%` operator unsupported** by comp(): prec_of lacks char 37, so the
   save_reg wrap `93 + ((ne-152) % 163)` stopped the parse at `%`.
   FIXED: prec_of gains `c == 37 → 3`, opcode dispatch gains `37 → 0x24`
   (IMod). Verified: `a % 3` now parses to end of source.
3. **main()'s `;`-separated body never block-flattened**: prep_core extracts
   main's body as main_expr but only fn bodies get flatten_blocks();
   the driver kept `let input = nperform("IO.read"); if ...` and comp()
   (no `;` support) stopped at the `;`. FIXED: prep_core main() now runs
   flatten_blocks + _flatten_block_body + convert_operators on main_expr.
   Verified: full prep source now parses to END (COMPDONE pos=23766/23767).

Status: checks 1-6 green after all three fixes; full self-build produces a
186-fn artifact whose driver runs (echoes source, reads input, prints
COMPDONE). 0x65 = 0.

## REMAINING BLOCKER (session 5 end) — capture threading in self-compiled artifact
- The self-compiled artifact's comp() receives ALL args as 0
  (`; CMP len=0 pos=0 nr=0 elen=0` from a probe on comp entry). The host
  runs the same source correctly (CMP len=48 on `add(3)(4)` input).
- comp is curried 8-deep (`fn(src) => fn(pos) => ... fn(elen) => body`);
  185 fn( in source = 185 FN_STARTs in artifact, so each `=>` level IS a
  separate fn. fn184 = comp's innermost body (4165 instrs, pc23218..27411).
- fn185 (the small driver) WORKS (reads IO, echoes, calls compile). So
  plain calls work; the 8-level curried call threading into fn184 fails:
  the innermost fn reads captures as 0 → args never arrive through the
  closure levels.
- The pre-existing 42-fn "working" artifact (bootstrap/self_compile.nbc,
  Sep 2 08:17) has the SAME bug: its driver prints `; driver start len=N`
  then `result=0` for every input — it RAN its driver but never compiled
  correctly. So this capture bug is the true long-standing blocker, not
  anything introduced this session.
- HYPOTHESIS for next session: capture-slot mismatch in the closure
  materialization of the 8-level curried comp — the fixup_hex fn-index
  assignment (LIFO stack pairing of FN_START/FN_END) may not match the
  VM's fn_table order (pc order) when fns NEST (main wraps helpers), so a
  Closure word gets the wrong fn index and the innermost level's closure
  captures come from the wrong slots. Test: compare the fn index fixup
  for comp's 8 levels against the VM's fn_table, and verify each level's
  closure materialization CapStore count matches its captured-var count.
- NEXT STEP when resumed: disassemble the artifact's closure materialization
  for comp's innermost fn and check the CapStore slots vs the remap_captures
  output; or run the artifact under the `debug` tool with a breakpoint at
  comp entry to inspect the captured register values.

## Session 5 addendum — fixup nested-Jmp fix + remaining capture bug localization
4. **fixup_hex fn-body-skip Jmp pairing bug (FIXED)**: `consume_next_fn_end_after`
   paired each fn-body-skip Jmp with the FIRST fn_end after it, but for NESTED
   fns (main wraps helpers; curried comp = 8 nested levels) the first fn_end
   after a Jmp is an INNER CHILD's end, not the Jmp's own fn. fn177's Jmp
   landed 100 instructions early. FIXED: build the FN_START→FN_END nesting
   map (stack-based) first, map each Jmp to the FN_START that follows it,
   and target that FN_START's paired fn_end. Verified: fn177's body-skip Jmp
   now targets its own Closure materialization (offset 4400 → pc 27239).
   (VM Jmp semantics: `pc = pc + offset - 1` with pc auto-incremented first,
   so offset = target - jmp_pc is correct — disasm's `-> pc+1+off` arrow is
   cosmetic; fixup's math was already right.)
5. **REMAINING BLOCKER — closure-capture threading in curried calls**:
   compile() receives src="1" len=1 correctly and emits a structurally
   correct 8-chain of ClosureCalls (r222→r229, each `Move arg->r10` then
   ClosureCall). But comp's INNERMOST body receives ALL ZERO args with
   src = `#Value(7ff7400000000013)` — a CLOSURE (fn#19's), not the string.
   pos/len/left/nr = 0. So intermediate curried levels capture WRONG values:
   level 1 (fn177) materializes level 2 (fn178) capturing garbage. The
   fn178 materialization (pc27138) CapStores r46-r59+r149 into slots — looks
   structurally right, but the captured VALUES are wrong at runtime.
   This bug affects the PRE-EXISTING 42-fn "working" artifact too (its driver
   printed result=0 for every input) — it is the true long-standing blocker.
- NEXT STEP (fresh session, needs VM-level debugging): use the `debug` tool
  on target/debug/nulang with the artifact /tmp/final.nbc, breakpoint at
  vm.rs OpCode::ClosureCall (line ~4700), and inspect what closure fn index
  and arg register each of the 8 chained calls uses, and what the
  intermediate closures' captured slots contain. Hypothesis: the closure
  materialization CapStore source registers (r46-r59 etc.) hold stale values
  because the SELF-capture emit (emit_word(0x62, vr, self_slot, save_reg))
  or emit_fnend_caps uses register numbers that collide with the live param
  staging at that point in the nested emission.

## Session 5 final — VM trace pinpoints the runtime bug (closure routing)
Using NULANG_TRACE=1 on the monolithic artifact (/tmp/mono.nbc, 27644 instrs):
- Entry Jmp (pc0) -> 27634 correct; main (fn#1) body runs (F1 at pc1).
- compile() executes, gets src="1" (r10 = heap-string ptr 7ffc...bb01b8 at the
  first comp ClosureCall, pc27459).
- The comp ClosureCall (pc27459, F2) jumps to **pc294 = fn9's body**, NOT
  comp's level-1 (fn177 at pc22840). The closure value in the call register
  is #Value(7ff7400000000000) = CLOSURE_ENV_FLAG|0 = closure-env index 0 —
  the SAME value sitting in r8/r9/r208/r210 from main's OWN entry
  materialization (pc27635-27641). comp's real closure (materialized at
  pc27239 `Closure fn#177 -> r58`) is NEVER in the register at call time.
- ROOT CAUSE: comp's closure materialization code at pc27239 (deep inside
  the nested fn-body region) is SKIPPED during main's body setup — the
  fn-body-skip Jmp routing jumps main's linear setup flow PAST the nested
  helper materializations, so the `let comp = ...` binding never executes
  its Closure instruction and r58/r222 hold a stale closure (env 0, from
  main's own entry dance).
- The linear setup flow that materializes the 47 helpers must reach EACH
  helper's `Closure fn#N` materialization in sequence, but the nested
  emission + body-skip Jmps skip over them. This is the same nesting class
  as the fixup Jmp bug but on the RUNTIME routing side: when main's body
  flows through the region containing a helper's [Jmp->fn_end, FN_START,
  body, fn_end:, Closure], the body-skip Jmp must route it to THAT helper's
  Closure (fn_end), and then CONTINUE to the next helper — landing on the
  Closure executes it, and execution must then fall through to the next
  helper's Jmp. If any Jmp lands one-past (skipping the Closure) or the
  nesting pairing is off, a helper never materializes.
- NEXT STEP (fresh session): with the corrected fixup (Jmps land ON their
  own fn's Closure materialization at pc 27239/27138/...), verify main's
  setup flow actually EXECUTES each Closure by tracing the region
  pc22839..27634: count how many `Closure fn#N -> rX` instructions execute
  during main's setup (should be ~47 helpers + compile + main). The
  trace will show which helpers' materializations are skipped. Fix the
  routing so every nested helper's Closure runs exactly once in order.

## Session 5 closing — final trace localization
- comp's OWN closure materializes CORRECTLY at pc27239: r58 = 
  #Value(7ff740000000001d) = CLOSURE_ENV_FLAG | env-index 29 (fresh env).
- But compile()'s comp-call site (pc27459, F2) invokes a closure that
  resolves to pc294 (fn9), NOT comp level-1 (fn177/pc22840). The value
  called is #Value(7ff7400000000000) = env-index 0 — main's entry-dance
  closure, NOT comp's env-29 closure.
- => compile()'s captured reference to `comp` points at the WRONG slot:
  compile's closure was materialized capturing env slot X, but at call time
  that slot holds main's env-0 closure (or was never populated with comp's
  env-29 closure). This is a capture-slot mismatch in compile()'s closure
  materialization: the remap/emit_fnend_caps assigned `comp` to a slot that
  the runtime fills with a different value.
- The chain of evidence is complete: 4 compile-time bugs fixed (hash, %, 
  semicolon, fixup Jmp nesting) + 1 architectural finding (splitter exceeds 
  i16 Jmp range, must stay disabled). The remaining runtime bug is a
  closure-capture slot mismatch for helper references inside compile()
  (and likely all helpers that reference comp). 
- NEXT (fresh session): trace compile()'s OWN closure materialization
  (find its Closure fn# pc) and compare the CapStore slot assigned to
  `comp` vs the slot compile's body CapLoads when it calls comp. The fix
  is in emit_fnend_caps / remap_captures slot assignment for the deep
  nested-let environment, OR in the env_push slot numbering for the
  47-helper + compile let-chain.

## Session 5 FINAL — trace correction: comp chain DOES execute correctly
Correction to the previous "closure routing" note. Detailed NULANG_TRACE
analysis of /tmp/mono.nbc shows:
- The 8-call comp chain (pc27404-27435, registers r220→r227) executes
  CORRECTLY. r220 = comp level-1 closure (env 46), src="1" in r10.
- First ClosureCall (pc27404) jumps to pc22840 = comp level-1 body (F3),
  NOT fn9. (The earlier "jumps to pc294" reading was pc27459, a different
  later call — comp's own internal recursion, which is legitimate.)
- Level-1 body materializes level-2's closure (CapStore into r60 at
  pc27138+), ends `Move r60->r0; RetVal` at pc27237-27238.
- The earlier COMPARG probe (src=closure, args=0) fired from the INNERMOST
  body — meaning by level 8 the threaded args have degraded. Since level-1
  demonstrably receives src correctly, the corruption happens between
  level 1 and level 8: an intermediate level's materialization captures a
  WRONG register for one of the threaded params (src/pos/len/...), so
  deeper levels read garbage.
- The frame trace shows F3 pc visiting 27238 (RetVal), then 27137, then
  27034 — nested materialization code executing within one logical level's
  flow, suggesting intermediate level bodies cascade through MULTIPLE inner
  materializations (each ending in its own RetVal) instead of cleanly
  materializing exactly one next-level closure and returning.
- REMAINING BUG (precise): in the emitted curried-level bodies, an
  intermediate level's body-flow executes the materialization code of
  SEVERAL inner levels (its own + deeper ones) before returning, so the
  closure it returns captures registers that deeper materializations
  clobbered. The fn-body-skip Jmp that should route a CALLED level's flow
  to exactly its own fn_end materialization is landing such that the flow
  runs through sibling materializations first.
- This affects the pre-existing 42-fn artifact identically (driver printed
  result=0 for every input). It is the true long-standing self-host blocker.

## Session 5 ULTIMATE — chain verified correct end-to-end; bug is in level-8 args
Clean frame-transition extraction from the trace proves the 8-level curried
comp chain executes PERFECTLY:
  pc27404 CALL -> level1 materializes fn178 @27138, RET 27238
  pc27408 CALL -> level2 -> fn179 @27035, RET 27137
  ... (levels 3-7 all clean: each materializes the next fn and returns) ...
  pc27435 CALL -> comp innermost body @25096 (F3), which runs comp's real
  parse logic calling many helpers (deep F4/F5 curried helper chains).
So routing AND materialization are correct. comp's body EXECUTES. The
result=0 comes from comp's body receiving wrong args at level 8: the
earlier COMPARG probe (src=fn19-closure, pos/len/left/nr=0) fires from
pc25096's body — the innermost level's captured params are garbage even
though level 1 demonstrably received src="1".
=> The corruption is in the intermediate closure CAPTURE VALUES: when level
N materializes level N+1, it CapStores the threaded params (src..) into
level N+1's closure slots, but one of those CapStores reads a register that
was clobbered (the materialization code itself uses high registers that
overlap the saved params). Level 1 stores src from r148 correctly (verified
`Move r148->r176; CapStore slot48`), but deeper levels capture from
registers that their own materialization prologue clobbered.
NEXT (fresh session, focused): for each level N in 1..7, compare the
register that level N saved its params into (Move r10->rNNN at level N's
body start) vs the register that level N+1's materialization CapLoads/CapStores
for those params. The mismatch register is the bug — likely the param-save
register (100+elen) collides with the 128+slot capture staging registers
used by emit_fnend_step, OR remap_captures assigns the wrong source reg for
params captured through multiple levels. Check emit_fnend_step's cap_dst =
128+slot against param_save = 100+elen ranges for overlap.

## Session 5 DEEP-DIVE — final root-cause chain (6 fixes, 1 remaining)
Additional fixes applied and verified this session:
5. **emit_fnend_step staging-register collision (FIXED)**: cap_dst = 128+slot
   overlapped the curried-level param-save registers (100+elen = r148-155)
   for closures with ≥20 slots — an earlier slot's staging write clobbered a
   later slot's source (the threaded src param). Fixed: CapStore directly
   from the source register, no staging Move. VERIFIED via CARGS probe:
   comp's innermost now receives len=1, nr=8, pos=0 CORRECTLY (was all-zero).
6. **fixup JmpF single-patch bug (FIXED)**: each if emits TWO JmpFs (int-zero
   + bool-false checks) but fixup recorded/patched only ONE (li+1, which also
   pointed at a '; 10' remark line instead of the instruction). Fixed: record
   ALL jmpfs per if/and/or frame, scanning forward to the real instruction
   line. VERIFIED: JmpF offset=0 count 25 -> 0; the step-limit loop is gone.
REMAINING (final blocker): comp's innermost now receives len=1 nr=8 pos=0
   but **left=0** (must be no_left = 1<<40). The 4th curried param threads
   wrong through levels 4-8. Root class: during main's LINEAR setup, closure
   materializations (emit_fnend_caps) read param-save registers r148-155
   that are UNINITIALIZED at setup time (they only hold values when the
   curried levels are CALLED). A setup-time CapStore reading r151 (pc12636
   `CapStore r63 slot=51 r151`) captures garbage because the env mapping
   counts param_save regs as captures but they aren't populated yet. The
   env's capture model (count_captures treats reg>=100 as capture) conflates
   param_saves (r148-155, call-time) with genuine captures (r11-r47,
   setup-time) — the deepest design issue in the curried self-host.
NEXT (fresh session): separate param_saves from capture-worthy registers in
   remap_captures/count_captures/count_existing_captures, OR move the
   param_save base below 100 (e.g. reuse r11+rX region) so materializations
   never read uninitialized param registers. When left threads correctly,
   comp should parse '1' -> Const1 (04080000) matching the host.

## Session 5 FINAL FIXES (7-8) + emit corruption remaining
7. **compile() no_left literal overflow (FIXED)**: compile() passed
   `comp(src,0,len,1<<40,...)`; prep rewrote 1<<40 to the literal
   1099511627776 (2^40). read_int packs values as acc<<16, so acc=2^40
   needs 2^56 > 48-bit payload -> overflowed to 0 -> comp got left=0 and
   never parsed. Fixed: compile() now passes 268435456*4096 (factors <2^32,
   product 2^40 computed at runtime). VERIFIED: comp("1") now returns
   result=8 pos=1 (EXACTLY matching the host compiler!). comp PARSES
   correctly now.
8. **REMAINING: emit_hex rendering corrupted in the self-compiled artifact**.
   comp parses correctly (result=8 pos=1 for '1') but the emitted hex is
   zeros: host emits `04080000 12080000 01000000`, artifact emits
   `00000000 00000000`. For '10' host emits `07000008`, artifact `00000010`.
   The instruction WORDS comp computes are correct (reg 8 etc.) but
   emit_hex renders them wrong. Suspects: (a) the division constants in
   emit_hex (0x10000000=2^28 etc.) hit the same read_int acc<<16 packing
   limit if any exceeds 2^32 (2^28<<16=2^44 OK; 2^24<<16=2^40 OK - all
   fit); (b) hex_digit's string concat breaks in the self-compiled code
   (the Unit-render `()` quirk: if any hex_digit returns Unit, concat
   collapses). Test: have the artifact emit a word with NO hex digit 10-15
   (all 0-9) vs one with `a`-`f`; check if `()` appears (fixup repairs to
   `a`) or true zeros. The `00000000` (all zero, no `()`) suggests emit_word
   computed w=0 OR emit_hex's divisions all returned 0 - check whether the
   artifact's instr() (opcode<<24 | op1<<16 | ...) overflows: 0x04<<24 =
   2^26 << 16? No - <<24 of a small opcode is fine (<2^48). But op1<<16
   where op1 up to 255: 255<<16 = 2^24 OK. All fit 48 bits. So the word
   computation is fine; the corruption is in emit_hex's string building.
   NEXT: probe emit_hex(w) inside the artifact with a known w to see the
   rendered string vs expected.

## Session 5 CLOSING — comp PARSES correctly; emit_word curried capture is final blocker
Definitive state after 8 fixes (all checks 1-6 green):
- comp("1") returns result=8 pos=1 — IDENTICAL to the host compiler. The
  entire parse pipeline (hash dispatch, %, semicolon flatten, fixup Jmp
  nesting, emit_fnend direct-capture, fixup JmpF multi-patch, no_left
  literal) now works end-to-end at the comp level.
- REMAINING: the ARTIFACT's emitted hex is all zeros (`00000000` for
  Const1/Move/Halt that should be `04080000 12080000 01000000`).
  emit_word/emit_hex render zeros. comp computes the right register but the
  instruction WORD reaches emit_hex as 0 (or hex_digit renders 0).
- emit_word is curried 4-deep: fn(opcode)=>fn(op1)=>fn(op2)=>fn(op3)=>
  (instr(opcode)(op1)(op2)(op3); emit_hex(w)). Same curried-capture class
  as comp's level-4 left=0 bug. emit_word is invoked from comp's DEEP body
  (high elen), so its curried closures likely capture wrong registers too.
- Adding ANY probe inside emit_word makes the artifact return `nil` (total
  failure) — emit_word sits on a knife-edge register allocation, consistent
  with its curried captures being wrong.
- NEXT (fresh session): apply the SAME class of fix that repaired comp's
  param threading to emit_word — verify emit_word's 4 curried level bodies
  save their params and materialize the next level capturing the right
  registers (the emit_fnend direct-capture fix may need to also cover
  emit_word's materialization, OR emit_word's param_save registers collide
  at the deep elen where comp calls it). Alternatively, de-curry emit_word
  (it's called everywhere; making it a single fn with all 4 args would
  remove the fragile threading) — prep_core's curry pass could special-case
  emit_word like it does nperform.

## Session 5 FINAL TRACE — emit_word receives w=0 (curried args arrive as 0)
NULANG_TRACE of /tmp/state2.nbc shows: emit_hex (F5) is entered with r10=0
(the word w). comp parsed "1" correctly (result=8) and calls emit_word(4,8,0,0)
for Const1, but by the time emit_hex runs, w=0. emit_word's 4 curried levels
receive 0 for every arg — the SAME curried-capture class as comp's left=0.
My emit_fnend direct-CapStore fix repaired comp's OWN 8-level threading
(comp now gets left=2^40 correctly), but emit_word's 4 levels are STILL
mis-capturing. emit_word is called from deep inside comp's body at high
elen, and its closures are materialized in that deep context.
NEXT: apply the same diagnosis to emit_word's materialization — find where
emit_word's curried closure is materialized and check its param_save
registers vs the CapStore sources. OR: de-curry emit_word in prep_core
(special-case like nperform: keep emit_word(op,op1,op2,op3) as a multi-arg
call AND define it as a multi-arg fn — but comp only parses single-arg
application, so the definition must be fn(opcode)=>fn(op1)=>... yet the CALL
sites could pass all 4 args if comp supported it... it doesn't). Realistic
fix: the emit_fnend fix may need the SAME treatment for emit_word's levels
(direct capture), OR the param_save base must move below the capture
staging range so NO curried level collides. The param_save registers
(100+elen) overlap the CapLoad staging (128+slot region reads r11-r47) only
when elen>=28; emit_word called at elen~50 puts param_saves at r150+ which
the materialization CapLoads read as capture sources.
Also fixed this round: replaced `1 << 39` (3 sites: is_bool_bit + two
dr+(1<<39) continuations) with `268435456 * 2048` — the 2^39 literal
(549755813888) also overflowed read_int's acc<<16 packing. Prep output now
has ZERO literals > 2^31. This bug class (large literals via convert_
operators' 1<<n expansion) is fully cleared from the source.

## Session 5 ABSOLUTE END — emit_word capture mechanics verified; value bug in slots 12-14
VM CapLoad semantics verified: slot=op1, dst=op2. emit_cap_step emits
(0x61, slot, 11+slot) so capture slot N loads into r11+N. fn14 (emit_word's
innermost/instr) correctly CapLoads slots 0-14 into r11-r25 and reads
opcode/op1/op2 from r23/r24/r25 (= slots 12/13/14). The CapLoad MECHANICS
are correct. The bug is the VALUES in emit_word's closure slots 12-14 at
materialization time: emit_word is curried fn(opcode)=>fn(op1)=>fn(op2)=>
fn(op3), and each level's closure must capture the prior params. When the
level-1 closure (holding opcode) is materialized and later called from
comp's deep body, the opcode value threaded into slot 12 arrives as 0.
Same class as comp's left=0 (fixed via emit_fnend direct-capture) but
emit_word's levels are materialized in a different context (top-level let
#17, 4 nested levels) whose capture still reads uninitialized/stale
registers. The remaining work is applying the emit_fnend-class fix
(direct capture from the correct source register at the correct time) to
emit_word's specific materialization, OR restructuring so emit_word's
opcode/op1/op2/op3 are not threaded through 4 closure captures.
ALL session fixes verified: checks 1-6 green; comp('1') parses to
result=8 pos=1 (byte-identical to host); prep output free of literals>2^31;
JmpF offset-0 count 0. Final blocker: emit_word emits w=0 (curried args
arrive 0) -> artifact renders 00000000 instead of 04080000.

## Session 5 HARD STOP — static chain verified correct; runtime capture values wrong
Exhaustive static verification of emit_word's 4 curried levels (fn11-fn14):
- L1(fn11,pc412): saves opcode->r112, CapLoads 12 slots->r11-22, Jmp 528
- L1 materializes L2: CapStore slots 0-11 from r11-22, slot12 from r112(opcode) ✓
- L2(fn12,pc427): saves op1->r113, CapLoads 13 slots->r11-23, materializes L3
  with slot13 from r113(op1) ✓
- L3(fn13): saves op2->r114, materializes L4 with slot14 from r114 ✓
- L4(fn14,pc460): CapLoads 15 slots, reads opcode from r23(slot12), computes
  instr = opcode*2^24 + op1*2^16 + op2*2^8 + op3 ✓
- VM CapStore: closure=op1, slot=op2, src=op3. emit_cap_step matches.
- VM CapLoad: slot=op1, dst=op2. Compiler emits (0x61, slot, 11+slot). ✓
The static capture chain is CORRECT at every level. Yet at RUNTIME emit_hex
is entered with w=0 (all 4 curried args arrive 0). The runtime capture
VALUES are wrong: emit_word's L1 closure (materialized during top-level
setup at pc543: slots 0-11 from r11/r101/r215-r233) captures whatever those
registers hold AT SETUP TIME. If the setup order hasn't populated r215+
with the right helper closures when emit_word's closure materializes, the
captures are garbage.
NEXT SESSION (definitive test): trace the RUNTIME values of emit_word L1's
captures when comp calls emit_word(4): (1) verify L1 is entered with
r10=4; (2) check L1's CapLoad of its slot-0..11 captures yields the helper
closures; (3) check the materialized L2 closure's slot-12 (opcode) actually
=4 after L1 runs. The failure point is one of these three. If L1's OWN
captures are garbage (its closure was materialized with wrong values during
setup), the fix is in the setup-order/materialization of top-level curried
definitions.

## Session 5 ULTIMATE — emit_word levels NEVER execute; binding resolves to emit_hex
Definitive trace finding: emit_word's 4 curried level bodies (pc412/427/443/
460 = fn11/12/13/14) NEVER execute — zero trace hits. Yet emit_hex (fn10)
runs. comp's `emit_word` reference resolves to fn10's closure (emit_hex),
not fn11's (emit_word L1). emit_word's self-closure IS correctly fn#11 at
pc543 (verified: fixup fn-index assignment is correct for the nested group
fn11-14, materialized at pc492/510/527/543 = fn14/13/12/11). So the bug is
in the TOP-LEVEL BINDING: the `let emit_word = ...` binding in the setup
chain captured/populated the WRONG closure (fn10's from pc397 instead of
fn11's from pc543) into the register comp later reads for emit_word.
NEXT (fresh): trace the top-level setup region where emit_word's binding is
processed — find the Move that stores the binding into its save_reg and
verify it reads r22 (fn11's closure from pc543) not r21 (fn10's from
pc397). The binding is processed in let-chain order; if emit_word's binding
executes BEFORE pc543's materialization (or reads the wrong register), it
grabs fn10's closure. This is the FINAL bug blocking check 7.

## Session 5 TERMINAL — emit_word binding resolves to emit_hex closure (env collision)
Final trace + disasm synthesis:
- fn10 (pc285-396) = emit_hex (8 digit-divisions + 8 hex_digit calls + IAdd concat + Perform). CORRECT.
- fn11 (pc412) = emit_word L1. Its levels (pc412/427/443/460) NEVER execute in the trace.
- emit_hex (fn10) DOES execute. emit_hex is only called from emit_word's innermost (fn14).
  Since fn14 never runs, emit_hex must be invoked DIRECTLY => comp's `emit_word` closure
  resolves to fn10's closure (emit_hex), not fn11's (emit_word L1).
- Bindings: emit_hex's closure stored r21->r231 (pc409); emit_word's stored r22->r233 (pc556).
  r233 ends up = #Value(7ff740000000000a) (env-10 closure). If env-10 = fn10's env, then
  emit_word's register r233 holds emit_hex's closure => emit_hex and emit_word bindings
  COLLIDED (same/sibling env slot), so comp's emit_word reference = emit_hex.
- Likely mechanism: top-level save_reg assignment collision OR the binding order stores
  fn10's closure into BOTH r231 and r233 (emit_hex's materialization CapStores/writes past
  its slot into emit_word's). Comp then calls emit_word->fn10(emit_hex) with opcode args.
NEXT SESSION (final, concrete): check whether emit_word's binding (pc556) reads r22 AFTER
r22 was overwritten, OR whether comp's env maps emit_word -> the register holding fn10's
closure. Compare emit_hex's materialization pc397-410 (Closure fn#10 -> r21, CapStores,
Move r21->r231) against emit_word's pc543-556. If emit_hex's CapStore chain (pc398-410)
writes r22 or r233, it corrupts emit_word's binding. This is the LAST bug.

## Session 5 FINAL ROOT CAUSE — instr arithmetic register collision (w=0 explained)
DEFINITIVE trace+disasm of emit_word's innermost (fn14/instr) for emit_word(4,8,0,0):
- pc478-479: opcode(4) x 16777216 -> r29 = 67108864 (opcode product) ✓
- pc480-482: op1(8) x 65536 -> r30 = 524288 ✓
- pc483: Move r25(op2=0) -> r29  *** CLOBBERS r29 (opcode product) with op2 ***
- pc484-485: op2(0) x 256 -> r31 = 0
- pc487-489: IAdd chain sums r29(now op2=0)+r30+r31 = 0 (opcode product LOST)
- => w = 0 passed to emit_hex -> "00000000" output.
ROOT CAUSE: register allocation in compile_hex's operator-precedence codegen for
the 4-term expression (opcode*16777216)+(op1*65536)+(op2*256)+op3. The op2
identifier lookup (Move r25->r29) targets r29, which holds the first product.
The chain's register assignment (operand staging at nr, products at nr+2)
collides for this expression shape. The HOST compiler (Rust) allocates
correctly; the SELF-COMPILED compiler's emitted bytecode for this expr has the
collision => a codegen divergence in compile_hex's arithmetic chain handling.
FIX DIRECTION: in compile_hex, binary-op chains must stage each new operand
into a register distinct from all live product registers. Look at the operator
branch: `let rhs = comp(...nr+1...); let dr = nr + 2; emit(op, lr, rr, dr);
comp(src, rp, len, dr, min_prec, dr+1, ...)`. For a left-assoc sum of products,
each product's dr and the NEXT term's operand staging (nr+1) must not overlap
prior products. Alternatively rewrite instr to avoid the deep chain: compute
products into explicit lets, or use (opcode*16777216) with the adds structured
so no staging register aliases a live product.

## Session 5 CONCLUSION — instr collision is inherent to compile_hex codegen
The let-rewrite of instr (`let hi = opcode*16777216 in let mid = op1*65536 in
let lo = op2*256 in (hi+mid)+(lo+op3)`) did NOT fix the runtime: the
self-compiled compiler flattens the lets back into the same colliding
multiply chain (fn14 still shows direct r30/r31 products with op2's staging
clobbering the opcode product). compile_hex's let-branch + operator-chain
register allocation reuses registers for a 4-term sum-of-products at the
nesting depth where emit_word runs. The instr expression cannot compile
correctly through the current codegen at that register depth.
The remaining work is a real codegen fix in compile_hex's register
allocation for operator chains (each operand staging and product register
must be distinct across a left-assoc chain), OR restructuring emit_word to
not compute a 4-term packed word (e.g., emit 4 separate byte-emissions via
simpler arithmetic). This is the FINAL blocker for check 7.
ALL session fixes stand verified: checks 1-6 green; comp('1') parses to
result=8 pos=1 (host-identical); the emit_word curried chain threads
(4,8,0,0) correctly; only the final instr word-assembly arithmetic
collides, yielding the 00000000 emissions.

## Session 5 MILESTONE — instr accumulator-chain fix makes '1' output HOST-IDENTICAL
The instr register-collision fix: rewrite instr as an accumulator chain
(let hi = opcode*16777216; let acc1 = hi + op1*65536; let acc2 = acc1 +
op2*256; acc2 + op3) instead of a flat 4-term sum-of-products. The flat
chain's operand staging clobbered the first product register at emit_word's
nesting depth. VERIFIED: echo '1' | self.nbc now emits EXACTLY the host
output: `04080000 12080000 01000000` (Const1->r8, Move r8->r0, Halt).
Byte-identical to the host compiler. This is the first time a self-compiled
artifact produces correct output.
REMAINING GAPS (longer expressions): '1 + 2 * 3' mis-emits: comp reads a
number as `; const -66596757` and emits ConstU const#0 (pool index 0) —
the const VALUE remark and the pool INDEX disagree. 'let x=42...' and
'not false' produce empty/wrong output. The single-digit '1' works; longer
expressions hit const-pool/read_int issues at deeper parse. NEXT: trace the
const emission for a 2nd number (comp's digit branch emits remark with the
value then ConstU with a pool index — the pool index assignment (fixup
const_markers dedup) mismatches for values seen after the first). Also
re-verify read_int for multi-digit (acc*10+digit) at depth.

## Session 5 FINAL STATE — milestone + remaining digit-branch register issue
MILESTONE VERIFIED: with the instr accumulator-chain fix, the self-compiled
artifact emits HOST-IDENTICAL output for input '1':
  04080000 (Const1->r8), 12080000 (Move r8->r0), 01000000 (Halt).
REMAINING: numbers followed by any continuation (even a trailing space:
'2 ') mis-emit as ConstU const#0 instead of Const2. Bare '1'/'2' work.
read_int returns the correct packed value ((2<<16)+pos); the corruption is
in comp's digit branch AFTER read_int — the num = packed>>16 extraction or
the num==2 dispatch reads a clobbered register when the parse continues.
Same register-collision class as instr (fixed) now manifesting in comp's
digit branch at continuation. read_int's own <<16 is fine (bare numbers
work); do NOT change read_int. The fix is in comp's digit branch register
handling (packed/num/q must survive until the emit).
Next session: examine comp's digit branch codegen for '2 ' vs '2' — the
trailing-space continuation shifts register allocation so packed's register
is clobbered before num=packed>>16 executes.
11 fixes verified this session (checks 1-6 green throughout):
hash mult 37, % operator, main ; flatten, fixup nested-Jmp, emit_fnend
direct-capture, fixup multi-JmpF, splitter disabled, 1<<40 rewrite,
1<<39 rewrite, instr accumulator chain, (read_int explored+reverted).

## Session 5 TERMINAL STATE — milestone verified; digit-continuation register bug final
FINAL VERIFIED STATE:
- Self-compiled artifact (/tmp/final2.nbc) emits HOST-IDENTICAL output for
  input '1': 04080000 12080000 01000000. Checks 1-6 green.
- Inputs '2' and '2x' (letter continuation) emit correctly (Const2 05080000).
- Inputs '2 ' (space), '2+', '22', '2 2' (any space/operator/digit
  continuation) mis-emit ConstU const#0 (07000008) — comp computes a
  garbage num value.
- Trace comparison shows '2' and '2 ' execute IDENTICAL instruction
  sequences through read_int's recursion (pc2487-2494: acc*10+digit, then
  CALL recursion). The divergence is only in JmpF outcomes at the num
  dispatch (pc2442+), driven by register VALUES that differ between the
  inputs — read_int's acc/packed value differs when the input has a
  continuation.
- CRITICAL FRAGILITY: adding ANY probe (even a remark) inside comp's digit
  branch or emit_word breaks the artifact entirely (returns closure/nil) —
  compile_hex's register allocation is at capacity and any added statement
  in hot paths collides. This is the root fragility behind BOTH the
  digit-continuation bug and the earlier instr collision.
- The remaining fix is a register-allocation robustness improvement in
  compile_hex (operator chains, digit branch, and general hot paths must
  not alias live registers when a continuation follows). This is a
  substantial codegen change requiring careful work.

## Session 6 — is_digit/and-or codegen fixes; operator-dispatch guard bug characterized
VERIFIED FIXES:
- **is_digit space-termination bug FIXED**: artifact now emits host-identical
  output for '2 ', '2x', '22' (numbers followed by space/letter/digit). Root
  cause: compile_hex's `and` codegen never materialized a false value on the
  short-circuit path — is_digit's false path returned stale r0 (the closure
  arg c, a truthy int), so read_int/is_alphanum consumed spaces/operators.
- **and/or codegen rewritten** (compile_hex.nula): short-circuit PRESERVED,
  but each and/or now emits a deterministic fill on the skipped path
  (`JmpF → or_right`; truthy path `Jmp → or_fill`; `or_fill: Const{0,1} rr`;
  rhs path `Jmp → or_cont`). Previously the skip path left the result
  register stale (never-computed rhs register).
- **fixup_hex.py extended**: new markers and_fill/and_cont/or_fill/or_cont
  with stack-based frame resolution for nested and/or; legacy and_end/or_end
  markers still accepted. Nested 3-level ors verified resolving correctly
  (chained cont-jumps skip all fills).
- **fixup_hex.py was corrupted by repeated edit-tool attempts** (collection
  loop deleted, marker lists duplicated, wrong indentation) and repaired with
  deterministic python line-slicing scripts; py_compile clean; checks 1-6
  pass through the repaired fixup.
- Checks 1-6 all green with the changed compile_hex/fixup (host pipeline
  exercises if/let/fn/recursion through the new codegen).
- Artifact /tmp/cg.nbc: host-identical for '1', '2 ', '2x', '22'.
REMAINING BLOCKER (check 7): comp's own binary-operator dispatch guard
`if prec == 0 or prec < min_prec then pair else { emit }` (compile_hex.nula
~line 811, artifact pc~20255, register depth r238+) STILL mis-evaluates:
NULANG_TRACE shows prec_of('+')=2 computed correctly, the 4-char
comparison-or-chain (c==61-or-33-or-60-or-62) resolving correctly to the
else branch, but the 2-term or-guard `prec == 0 or prec < min_prec`
returns pair (stop) instead of proceeding to the binary-op emission — so
'1+2', '1 < 2', 'let ...', 'if ...' all still stop after the first primary.
This is the same compile_hex register-allocation-capacity bug class
(instr fix, digit branch) surfacing in compile_hex's own deepest compiled
code. Fix requires allocator surgery at extreme depth or restructuring the
guard source; each compile_hex edit risks artifact collapse and costs a
~15s rebuild + full re-verification.

## Session 7 — -1 literal misparse + pack-collision fixes: operators/parens/not now host-identical
MAJOR FIXES (all verified host-identical on the rebuilt artifact /tmp/p3.nbc):
1. **-1 literal misparse (THE operator blocker)**: compile_hex cannot parse
   unary minus. All 10 `-1` min_prec arguments in comp-recursion calls were
   miscompiled: compile()'s `-1` became `Const1; ISub(4096, 1) = 4095` (the
   leftover 4096 from the no_left arg!), so min_prec was 4095 → the binary-op
   guard `prec < min_prec` was ALWAYS true → every operator expression
   returned pair (stopped after the first primary). Fixed: replaced all 10
   `, -1, ` with `, 0 - 1, ` in compile_hex.nula.
2. **Pack-expression register collisions** (same class as the original instr
   fix): flat multi-term `(x << 18) + (y << 17) + z` and `(x << 18) + (y + 1)`
   chains let a later term's CONST emission reuse a live product register.
   Fixed with accumulator/staged lets at 3 sites:
   - comp's left!=no_left pair pack `(rl<<18)+(lb<<17)+pos` (line ~687)
   - paren-primary `(rv<<18)+(q2+1)` (staged q3 = q2+1 first)
   - not-codegen `(dr2<<18)+(1<<17)+ip`
   RESULT: artifact now emits HOST-IDENTICAL bytecode for '1', '1+2',
   '1 + 2 * 3' (precedence), '2+3', '5-3', '1 < 2', '(1)', '((1))', '(1+2)',
   'not false', '2 ', '2x', '22'. Checks 1-6 green.
REMAINING (check-7 expressions still DIFF, each a distinct deep-depth
codegen collision in the same class, artifact pc regions ~r230+):
- 'let x = N in x': let-codegen save_reg Move emits corrupted opcode
  0x12→0x32 and dst 0xd0→0x09 (save_reg=200+ne=208 lost; instr accumulator
  collision at the let emission depth).
- 'if ...': if-codegen then-body position wrong when cond is a comparison
  ('if 1 < 2 then 100...' reparses the cond as the then-body); literal
  consts in then/else also mis-emit.
- 'A < B and C < D' / 'A > B or C > D': the comparison continuation's
  pair loses its bool bit (lb) when a following and/or keyword is seen —
  and/or-codegen takes the lb==0 path with wrong registers (lr=rr not dr).
- fn/lambda bodies + recursion: fn body codegen emits truncated bodies.
Artifact saved at /tmp/selfcompile_p3.nbc. Pipeline to rebuild after edits:
prep_core.py → compile_hex (NULANG_STEP_LIMIT=400000000) → fixup_hex.py →
hex2nbc.py.

## Session 7b — and/or + if deep-dive: 21/27 matrix expressions host-identical
Matrix on /tmp/selfcompile_p4b.nbc (built from current source): 21 PASS / 6 FAIL.
PASS includes all arithmetic/precedence/cmp/paren/not/ident-continuation cases plus
'1 < 2 + 1', '1 + 2 < 3', '1 < 2 < 3' (comparison continuation + binary/chained
follow-ops WORK), '1 or 2', 'true or false' (or-lb0 WORKS), '1 and 100', '100 and 2'
(and with a ConstU rhs WORKS).
FAIL: '1 and 2' (and-lb0, small-const rhs), 'let x = 1 in x', 'if true then 1 else 2',
'if 1 < 2 then 100 else 200', '3 > 2 or 1 > 2' (or-lb1/comparison lhs),
'(fn(x) => x + 1)(41)'.
DIAGNOSTIC FINDINGS this session:
1. and-lb0 ('1 and 2'): artifact emits byte-identical stream through the fill
   (Const0 r9) then the FINAL continuation returns pair reg 7 instead of 9
   (COMPDONE result=7 vs 9) -> compile() Moves r7->r0. rr (9) is live across
   remark("and_cont:") + the final comp call and its register dies there.
   or-lb0 (same shape, Const1 fill) works: only structural difference is or has a
   Jmp between its JmpF and the rhs comp; and runs rhs directly after JmpF.
   Additive diagnostics (remark with Int.to_string of lr/lb/nr/pair) COLLAPSED the
   artifact (no output at all, even for previously-working '1 or 2') - any source
   change near the and codegen shifts the register cliff.
2. Restructuring and-codegen to or's exact hop topology (Jmp->and_rhs before rhs)
   COLLAPSED the artifact: even '1' diverged. Reverted. The and codegen site sits
   exactly on an allocator cliff; near-site edits collapse the build.
3. or-lb1 / and-lb1 (comparison lhs): comparison continuation works for binary and
   chained-cmp follow-ops but the comparison's OWN ICmpLt emit fires AFTER the
   and/or codegen with garbage rr (deferred emission); and/or sees pair lb=0 with
   lr=rr. Pre-binding the continuation args (drp/drn lets, both cmp sites) changed
   nothing observable -> kept (harmless, conservative).
4. 'if 1 < 2 then 100 else 200': artifact INFINITE-LOOPS re-parsing '1 < 2' with
   ratcheting registers (Const1 rx Const2 rx+1 ICmpLt JmpF repeating, rx ascending)
   - if-codegen reads a corrupted cond position after a comparison cond.
5. 'if true then 1 else 2': then_body comp hits the no-emission fallback
   ((nr<<18)+p, tr=nr=9) and the then-Move goes to r11 (bf reg) not result_reg r10
   - tp/result_reg values corrupted in the artifact's if-codegen.
6. fn is ALREADY extracted to comp_fn_parse (like comp_nperform_parse) yet
   '(fn(x) => x + 1)(41)' still truncates - weakens the "extract codegen to fresh
   function frames" hypothesis; the hazards are subtler register-value deaths across
   calls in the artifact's compiled deep code, not pure nesting depth.
NEXT-STEP GUIDANCE: each remaining site (let save_reg, if tp/result_reg, and rr,
fn body) shows a live codegen value dying across an intervening call. The artifact
collapses on near-site source edits, so fixes must (a) be far enough from the site
to not shift its allocation, or (b) change ALLOCATION wholesale via a preceding
neutral-size edit, or (c) revisit the host-side suspicion: verify whether the HOST
compiler miscompiles some construct used ONLY in the failing codegens (e.g. deep
nested `if ... then ... else ...;` statements after other statements, or
multi-binding let sequences) - compare host compile_hex.nula vs host compile of a
minimal repro to check for a host bug rather than an artifact bug.

## Session 7c — cprec root cause FIXED + structural root cause of all remaining DIFFs
MAJOR FIX (advisors' parse_cmp/parse_shift lead, CONFIRMED): parse_cmp/parse_shift
returned flat 3-term packs `(1 << 16) + (op << 8) + len` whose (1<<16)/(3<<16)
PRECEDENCE term was zeroed in the artifact -> cprec/sprec = 0. Consequence: the
comparison's rhs sub-comp ran with min_prec = 0, so the following `and`/`or`
(precedence 0) dispatch `if 0 < min_prec` FAILED and the keyword was ABSORBED BY
THE RHS COMP at nr+1 (lr = rr = the rhs reg, exactly the observed IEq r9 r7 in
'1 < 2 and 3'). Fix: `fn cmp_pack(prec, op, clen)` staging prec<<16 / op<<8 /
hi / hi+clen into stepwise lets; all parse_cmp (6 ops) + parse_shift (2) sites
return cmp_pack(...). RESULT (artifact /tmp/selfcompile_r1.nbc): ALL comparison-
lhs and/or + chained + <=/>=/==/!= combos AND plain 'and'/'or' host-identical.
Matrix: 31 PASS / 6 FAIL (was 21/27). Checks 1-6 green.
STRUCTURAL ROOT CAUSE OF ALL REMAINING DIFFS (let/if/fn/not):
bootstrap/prep_core.py's transform_fns_for_self_host wraps the ENTIRE compiler as
ONE giant expression `let main = fn() => (let low17 = fn ... in ...)` — a single
module function holding 187+ nested fn closures and THOUSANDS of let locals.
The VM register file holds 239 locals (LOCAL_BASE=15..253); everything beyond
spills through r12-r14 round-robin spill temps (src/mir_codegen.rs). Consequences:
1. ANY pre-comp source change shifts which locals spill -> the observed layout
   whack-a-mole (cmp_pack fix flipped 'not false'; parse_shift staging flipped
   plain 'and'; every layout trades not-vs-and).
2. let/if/fn codegens (deepest, most spill pressure) fail in EVERY layout tried
   across sessions - they are register/spill-corruption, NOT compile_hex logic
   bugs (the HOST running compile_hex.nula as 51 real top-level functions is
   correct; only the single-function self-host form corrupts).
3. p6-family states: and/or complete, 'not false' regresses (iv=0x40-opcode
   read, dr2 10->20, pos leaks 2^17). q4-family (parse_cmp inline-staged, no
   helper): 'not false' OK but and-tail rr dies at higher rr values.
NEXT-STEP GUIDANCE (the real fix paths):
(a) Investigate src/mir_codegen.rs spill scheduling: a spilled local live ACROSS
    a call or across another SpillLoad/SpillStore, held in the round-robin
    r12-r14 temp, gets clobbered. The single-function form (thousands of
    locals) is the stress test. Verify with a minimal repro: one fn with
    ~250 lets + a cross-call-live spilled local.
(b) Alternatively avoid the single-function transform: make prep_core emit real
    top-level fns (requires compile_hex to parse top-level fn decls in its
    input, or a different self-host input shape) so each compiler fn gets its
    own frame and register file.

## Session 7d — state verified clean; host-codegen investigation
- compile_hex.nula VERIFIED INTACT (52 fns incl. comp/parse_cmp/cmp_pack; a prior
  shell-grep false-negative triggered a corruption alarm). Current source
  reproduces /tmp/r1.nbc byte-exactly (cmp re-run). src/vm.rs clean (JIT restored;
  the no-JIT experiment conclusively ruled out the JIT - corruption reproduces in
  the pure interpreter).
- Spill machinery in src/mir_codegen.rs verified sound: spill threshold =
  FUNC_VALUE_REG-LOCAL_BASE = 239 locals; unspilled locals map to r15..r253
  (no u8 wrap); spilled locals never hold physical registers (SpillLoad/Store per
  access, round-robin r12-r14 read temps, protect_dst for dst-in-spill-zone).
- Host many-locals probe (/tmp/probe_many.nula: 231 sequential lets with v5 live
  across a final call) returns CORRECT 6 - general high-local-count functions with
  cross-call liveness compile fine on the host.
- CONCLUSION: the let/if/fn corruptions are specific to the prep_core
  single-function transform shape (187+ nested lambdas inside one giant let-main),
  NOT general spill handling. Deterministic wrong values (save_reg=9/10,
  iv=0x40, rr=11) across all layouts = deterministic host-codegen misbehavior for
  THAT compiled shape at those source positions, not source logic bugs (host
  interpreting compile_hex.nula as 51 top-level fns is correct). Narrowing
  further requires inspecting the compiled lambda bodies' register/closure
  layout for the specific failing regions.
- BEST STATE (locked): cmp_pack-fixed source == /tmp/r1.nbc == /tmp/selfcompile_r1.nbc.
  Matrix 31 PASS / 6 FAIL ('not false' regression + let x2 + if x2 + fn). Checks
  1-6 green. fixup syntax clean. All comparison-lhs and/or + chained + <=/>=/==/!=
  + plain and/or host-identical.

## Session 7e — ROOT CAUSE DISCOVERED + reverted fix attempts; baseline locked
- STRUCTURAL REFRAME: the artifact .nbc is COMPILE_HEX-EMITTED, NOT Rust-host-
  compiled. The transformed source (prep_core output, `fn() =>` dialect) is
  REJECTED by the real Rust parser ("Unexpected token: =>"), so the artifact =
  compile_hex's own bytecode emission of the giant let-chain, run on the Rust VM.
  All prior src/mir_codegen.rs / spill / plan_drops analysis = IRRELEVANT to the
  artifact's corruption (they analyze code the Rust compiler never produced).
- Artifact structure (decode): 189 functions (fn table; main = fn 187, 3872
  instrs; comp = fn 187 region too; the let-codegen lives at module ~18258+).
  Trace "F#" = frame VEC SLOT (reused), PC = module-relative instruction offset.
- ROOT CAUSE: compile_hex self-emission at deep env nesting. Nested-let chains
  are CORRECT to depth 96 on the host and BREAK at depth 97+ (deterministic).
  Two bug classes confirmed at host level:
  (a) save_reg bank overlap: f(ne)=200+ne (ne 0-55) overlaps f(ne)=160+(ne-56)
      (ne 56-151) — f(96)=200=f(0) — old env-reachable bindings alias new ones
      (probe: 97-deep chain reading a0 returns a96's value).
  (b) name→keyword hash collisions: read_ident hashes are weak; 'a96' hashes to
      3884 = the 'fn' keyword → a var named a96 misdispatches to comp_fn_parse.
- Artifact-level trace evidence: comp's own let-branch var save_reg (value 208)
  held in artifact reg r104 (f(163)=93+11=104 — %163 branch, env depth ~163) got
  CLOBBERED by the compiled `0-1` min_prec arg computation (ISub 0,1 → r104) —
  the monotonic nr threading pushed temps into the x-binding bank.
- FIX ATTEMPTS BOTH REGRESSED THE ARTIFACT (must validate at artifact level!):
  (i) let-body nxt = nr+1 → nr (register reuse): 0/37 matrix (was 31/37).
  (ii) save_reg f(ne) re-tiled to 40+((ne-56)%160): 5/37.
  The register scheme is ENTANGLED with closure/capture ranges (11-19, 100-127,
  param-saves 100+elen), remark-fixup markers, and the current allocation shape;
  naive edits break the emitted self-code globally. REVERTED to byte-exact r1
  baseline (31/37, checks 1-6 green, /tmp/pH.nbc == /tmp/r1.nbc).
- NEXT: map the full register/capture model (comp_fn_closure param_save, capture
  slots 11-19/100-127, fixup frame markers) BEFORE any register change; validate
  every compile_hex edit against BOTH the 37-matrix AND the artifact rebuild; or
  restructure prep_core so no compiled fn exceeds ~90 env depth (the observed
  host correctness limit).

## Session 7f — TRUE depth limit + collision mechanics pinned; fix design specified
- KEY CORRECTION: the "breaks at depth 97" was an ARTIFACT of my chain naming.
  Chain vars a{i} include 'a96', and hash('a96') = 3884 = hash('fn') (hash = h*37+c
  mod 65536, read_ident line 167). The let-codegen's is_fn check reads the RHS's
  leading ident and tests kw>>16 == 3884: any let whose RHS starts with an ident
  hashing 3884 ('a96') misdetects as a recursive fn and emits the CapStore self-
  patch → "CapStore target is not a closure". ALL N=97 test chains contained a96
  as an RHS source → all broke → fake "depth 97 limit".
- REAL host depth limit (collision-free q-prefix names): correct at N=200, breaks
  at N=250 (pc986) = 256-register-file exhaustion (temps grow ~1 reg/let via the
  nxt=nr+1 threading).
- COLLISION INVENTORY (h*37+c mod 65536): keyword hashes let=20633 fn=3884 if=3987
  not=23741 nperform=64877 then=58195 else=25281 and=5891 or=4221 true=6932
  false=15187. 'a96'→3884. ZERO pairwise or keyword collisions among compile_hex's
  own ~327 candidate variable names → the artifact's corruption is NOT name-hash
  driven; it is the register tangle below.
- ARTIFACT REGISTER MECHANISM (final model): pD fn bodies = ~155-190 nested lets.
  Per fn, temps (r1's nxt=nr+1 threading) reach ≈ the let index (~190 at the end);
  x-bindings sit at f(ne) with ne = nr + elen ≈ 2×let-index, wrapping into
  93+((ne-152)%163) at let-index ≥ 76 → x's land at 93-135 while live temps cross
  the same band → deterministic temp-x register collisions in every deep codegen
  (let/if/fn/not regions). Shallow paths survive because their code sits at lower
  env depth (collision-free zone).
- FIX DESIGN (validated conceptually, NOT yet artifact-safe): (1) temp reuse —
  body/nested comps restart at the RHS base register instead of nr+1 (bounds temps
  to ~8-40); (2) retile x-bank f(ne) injectively ABOVE the temp ceiling and
  OUTSIDE the closure/capture ranges (11-19, 100-127, param-saves 100+elen) — the
  earlier %160 retile (5/37) and nxt=nr alone (0/37) each broke the artifact via
  layout churn/capture-range violations; the correct retile keeps branch1's 200-255
  (params, ≤56 bindings) and tiles later bindings through the safe gaps below 200.
  EVERY compile_hex edit must be validated against the artifact 37-matrix + full
  rebuild (host-level success does NOT predict artifact behavior — layout shifts
  trade which collisions fire).
- BASELINE RE-LOCKED: byte-exact r1 (31/37), checks 1-6 green, /tmp/pH2.nbc ==
  /tmp/r1.nbc. Keyword-char-guard fix (verify ident chars at dispatch) remains a
  worthwhile small fix for input vars that hash to keywords ('a96'), but it does
  NOT move the oracle.


## Session 7g — two-part register fix rejected; not-branch extraction locked at 32/37
- TWO-PART FIX (nxt=nr + save_reg tile [200,255]/[128,199]) applied as one change.
  Host q-chains: N=150/200/250 all OK (was N=250 fail). Artifact: 1285-byte stub,
  driver dropped (TAG_CLOSURE), 0/37. Same class as nxt=nr-alone. REVERTED;
  compile_hex.nula byte-reproduces r1.
- prep_core let-depth measured: main helper chain = 49 consecutive lets; deepest
  *named* helper inner chain = compile 7 / comp 5. The 155-190 figure is
  flatten_blocks turning `fn comp`'s 134 lets + 202 semis into ~199 nested lets
  *inside* the if-then atoms (hidden from a let-chain walk).
- Full split_comp_for_self_host: 69 fns, 48693 instrs > i16 Jmp (32767). Confirmed
  the disable comment. Do not re-enable.
- Narrow extract of let+if+not+infix_call: 7/37 (infix dead).
- Narrow extract of let+if+not: 9/37. Side-effect: `not false` OK and `if true`
  OK, but all arithmetic/and/or died. Extra 10-param curried helpers in the main
  chain churn the infix layout.
- Narrow extract of NOT ONLY (`split_comp_deep_kws`, DEEP_KWS=comp_kw_not):
  **32/37** (was 31/37). `not false` host-identical. Infix/and/or/cmp intact.
  instrs 22851 < 32767, 0x65=0, checks 1-6 green. Artifact /tmp/pNot.nbc
  == /tmp/pNot2.nbc. THIS IS THE NEW LOCKED BASELINE.
- Adding if on top of not: 22/37, all comparisons died. Reverted to not-only.
- Remaining DIFFs (5): `let x = 1 in x`, `let x = 42 in x + 1`,
  `if true then 1 else 2`, `if 1 < 2 then 100 else 200`, `(fn(x) => x + 1)(41)`.
- NEXT: extract let/if the same way BUT without a 10-param curried helper in the
  main chain (e.g. zero-arg thunks inside comp that capture, or one helper at a
  time with a smaller arity). Do not combine let+if+not. Do not retile save_reg
  / nxt=nr (artifact driver-drop). Do not full-split comp.


## Session 7h — remaining 5 DIFFs diagnosed; gated-nxt rejected; r2 re-locked
Advisories weighed: do not add let/if shards; do not thunk-wrap; do not retry
global nxt=nr / 128-band retile; param_save=100+elen roams 100–255 so a static
x-bank cannot miss every param-save. Kept not-only 32/37.

Forensic (artifact /tmp/r2.nbc vs host compile_hex, remarks+hex):
- `let x = 1 in x`: ART COMPDONE result=116 pos=1 (host result=9 pos=14).
  Hex starts Move r8→r1; Move r116→r0; Halt — no Const1. Let-branch is entered
  (otherwise result would be r8) but RHS is not emitted and save_reg dies
  across the RHS rec-call (emit uses 1, return pack uses 116=wrap-band).
- `if true then 1 else 2`: if/then remarks fire; `end:` missing → `eh==25281`
  (`else`) fails. pos=0. Truthiness uses r1–r4 instead of host r10–r13
  (nr-derived locals clobbered). Then/else bodies are Moves of r0/r8, not Const1/2.
- `if 1 < 2 then 100 else 200`: cond prefix matches through JmpF; then ConstU
  dest r1 not r9; tail garbled opcode 0x74.
- `(fn(x) => x + 1)(41)`: FN_START/fn_end fire; missing `const 41` remark;
  no ClosureCall. Body drops `+ 1`. pos=10 vs host 20.
Common class: artifact-frame locals of the still-monolithic let/if/fn codegens
(save_reg, no_left, nr-derived temps, else-hash, application save_reg) die
across nested `comp()` calls. `not` survives because it lives in extracted
comp_kw_not (8 lets, fresh elen).

Tried `let nxt = if elen < 50 then nr + 1 else nr` (keep main-chain nr+1,
reuse only in deep nested fns). Host OK; artifact 1285 B driver-drop (same
TAG_CLOSURE tail as global nxt=nr). The extra `if` in the let-continuation
is enough layout churn to drop main(). REVERTED. compile_hex.nula matches
r1; prep_core not-only extract intact; /tmp/pR2b.nbc == /tmp/r2.nbc; checks
1-6 green.

NEXT: do not touch nxt/save_reg/prep flatten. Remaining lever is making
let/if/fn codegens as shallow as `not` without a second 10-param helper on
the main chain (that shape already killed infix at 22/37). Or restage
save_reg/no_left in those codegens via a helper that already exists on the
chain (cmp_pack-style), not a new fn.


## Session 7i — no_left staging rejected; artifact ALPHA unreadable without driver-drop
- Temporary SENT/NL/ALPHA remarks: host on `let x = 1 in x` prints SENT=2^40,
  NL left=nl=2^40 nr=8 elen=0, ALPHA h=20633 q=3 p=0 (let arm WOULD match on host).
  Artifact of that tree: TAG_CLOSURE x4 + nil (driver drop). Cannot read artifact h
  via remarks without churning `comp`/`compile` enough to drop main().
- Staged `268435456 * 4096` as `let a = 268435456; a * 4096` at BOTH compile() and
  comp() no_left sites (no remarks). Driver kept (22857 instrs, ClosureCall tail).
  Matrix 21/37: all comparisons died; let still COMPDONE pos=1 result=101. REVERTED.
  /tmp/pR2c.nbc == /tmp/r2.nbc, checks 1-6 green.
- Conclusion: first-call no_left mismatch is NOT the let pos=1 cause (if-true already
  entered primary on r2). Staging the sentinel still churns cmp codegen. Next probe
  of artifact `h` must not add statements inside `fn comp` or `fn compile`.


## Session 7j — let-only in-place thunk kept at 32/37; let arm now emits Const1
- Additive `(fn() => { let-arm })()` wrap via thunk_wrap_let after not extract.
  Does not add a main-chain helper. Artifact /tmp/r3.nbc (95611 B, 22989 instrs).
- Matrix still 32/37 (infix intact). Checks 1-6 green.
- Let COMPDONE now result=9 pos=13 (r2 was result=116 pos=1). Hex:
  host `Const1 r8; Move r8→r208; Move r208→r9; Move r9→r0`
  art  `Const1 r8; Move r8→r1;  Move r9→r0`
  save_reg still dies inside the thunk (208→1); `in x` lookup Move missing.
- if-thunk not applied (script error; not retried this session).
- NEXT: restage save_reg after the RHS rec-call *inside the already-thunked
  let-arm* (recompute f(ne) immediately before emit_word), or thunk the `in x`
  lookup only. Do not add if as a 10-param helper.


## Session 7k — save_reg restage after RHS; lookup Move still missing
- Kept let-thunk + not extract. Did not add is_let, not+let DEEP_KWS, or if-thunk.
- After RHS rec-call, recompute sr_s = f(nr+elen) and env_s from captured
  nr/elen/env/q. emit_word uses sr_s not the pre-call save_reg.
- Let hex now: host `Const1 r8; Move r8→r208; Move r208→r9; Move r9→r0`
  art  `Const1 r8; Move r8→r208; Move r9→r0`  (save_reg 208 lives; lookup Move gone)
- COMPDONE still result=9 pos=13 (host pos=14). `in` branch not taken: vp from
  the RHS pair is 13 (the last `x`) not 9 (after `1`). env rebuild / q4 restage
  immediately before body/RHS did not change hex; reverted those two as dead.
- Matrix 32/37, checks 1-6 green. Artifact /tmp/r4.nbc == /tmp/pSr.nbc (95963 B).
- NEXT: why RHS `comp` returns pos=13 not 9 — pair low17, not save_reg. Do not
  add helpers. Restage/recompute the bound position, or find ` in ` from q.


## Session 7l — env_rec body; lookup still missing; 32/37
- Did not replace let-thunk with 10-param extract. No find-`in` scan
  (result=9 is nxt; `in` ran). No hash-120 probe on the tree (dropped driver).
- Dropped ident re-read (q2s/vh_s/env_s). Emit Move vr→sr_s immediately after
  sr_s = f(nr+elen). Body uses pre-call env_rec.
- Let hex unchanged: Const1 r8; Move r8→r208; Move r9→r0. Lookup still missing.
  env_rec vs env_s did not matter. Artifact /tmp/r5.nbc == /tmp/pE.nbc.
- Matrix 32/37, checks 1-6 green.
- Remaining: env_lookup(`x`) at nxt=9 returns unbound. env_rec likely dies
  across the RHS rec-call (string local). Next must bind `x` in an env that
  survives without extra lets in the thunk (those drop the driver).

## Session 8 — ORACLE COMPLETE: check 7 enabled, 37/37 + 6/6
- Root causes (all register-aliasing, cmp_pack class):
  1. env_push v = (low16(h)<<8)+low8(reg): artifact summed products into the
     same temp -> 400 (0x190) instead of 30920 (0x78C8). Staged h16/shifted/r8/v.
  2. env_decode (b0<<18)+(b1<<12)+(b2<<6)+b3: same aliasing. Staged t0/t1/t2/s01/s012.
  3. if true then 1 else 2: truthiness chain (~10 emits) clobbered cnext/tp/ep
     for the lb==0 path; `if 1 < 2` (lb==1 short path) always passed. FIX= extract
     if branch as comp_kw_if (DEEP_KWS).
  4. fib recursive closure: application-branch save_reg f(ne) miscomputed at
     depth (host r218 vs art r161). FIX= extract infix_call branch as comp_infix_call.
- Also fixed prep_core bug: `fns, main_expr = transform_fns_for_self_host(fns)`
  ignored second return value -> main_expr was None; changed to `fns, _ =`.
  split_comp_shards true-branch hash 6932 -> 18036 (2 sites) to match source.
- thunk_wrap_let now emits `(fn() => { ... })()` (was `(fn() {`).
- verify.sh check 7 ENABLED (export NULANG_STEP_LIMIT=400000000; build
  self_compile.nbc via prep|compile_hex|fixup|hex2nbc; 6 expect_oracle PASS).
- Matrix 37/37; checks 1-6 green; oracle 6/6. Commit a63e88c pushed.

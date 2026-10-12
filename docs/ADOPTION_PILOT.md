# Nulang independent-developer adoption pilot

**Status:** Proposed pilot protocol (not evidence of completed recruitment or adoption).  
**Tracker:** [Issue #809](https://github.com/nulang-org/nulang/issues/809)  
**Scope:** Nulang alpha, reference bytecode runtime. This is a developer experience evaluation, **not** a production reliability certification.  
**Recording template:** [Per-participant evidence worksheet](ADOPTION_PILOT_REPORT_TEMPLATE.md) (blank; no participants have been enrolled by this document).

## Goal and gate

Recruit **three developers who do not maintain the Nulang compiler** and have not been coached through the implementation. Each person should build or materially extend one useful Nulang application, then independently revisit it for a change or bug fix.

The pilot passes the adoption gate only when **three independent developers finish non-trivial applications**, or when **one production-like reference application** demonstrates a reproducible material advantage over an equivalent conventional stack. Merely starting an example or starring the repository does not satisfy the gate.
The separate [semantic stabilization contract](SEMANTIC_STABILIZATION_CONTRACT.md)
calls for at least **five** non-maintainer applications before broad
Stable/Frozen surface expansion. That later language-maturity gate is
**not** automatically met when this initial three-person adoption pilot passes.

## Paths a participant can choose

1. **Discovery:** Run the [Core-only browser playground](https://nulang.org/playground/) without an account or local setup. It cannot run actors, external networking, or durable native runtime behavior.
2. **Local use:** Install a checksummed [tagged release](https://nulang.org/getting-started/installation/) for a supported platform, if a corresponding release artifact is available, then open the [quick start](https://nulang.org/getting-started/quick-start/).
3. **Contributing:** Follow the [source-build guide](https://nulang.org/getting-started/installation/#build-from-source-contributors-and-custom-profiles) only if modifying the compiler or testing an unsupported platform/profile.

Ask each participant to narrate where instructions are confusing. **Do not repair their environment for them until they have recorded the blocker.**

## Pilot setup and fixed tasks

Before inviting anyone, record the exact current install instructions,
available release assets, example filenames, and their Git commit SHA.
If the native binary or a required hosted resource is not accessible,
report that as an onboarding blocker; do not silently switch participants to
a different installation procedure.

Give participants this task briefing without private coaching or pre-written
solutions. Keep initial assistance to clarification; when the participant is
blocked, record the original problem and elapsed time **before** giving help.
For comparable observations, select one of these two initial tracks and record
the choice in the [worksheet](ADOPTION_PILOT_REPORT_TEMPLATE.md):

- **Track A — useful CLI:** begin with
  [`examples/16_realworld.nula`](../examples/16_realworld.nula)
  or the **sequential** HTTP/JSON reporter
  [`examples/17_actor_fetcher.nula`](../examples/17_actor_fetcher.nula).
  Modify input handling, error behavior, data transformation, or output for
  a genuine use case. This track uses native HTTP/FS effects and may require
  external network access. The latter filename is historical; it does **not**
  demonstrate actors because its source documents a current actor/HTTP
  interaction limitation.
- **Track B — isolated actor state:** begin with the native
  [`examples/08_actors.nula`](../examples/08_actors.nula) and add a
  user-visible business invariant, for example a rejection case when a
  counter would exceed a self-chosen bound. Use actor message handling rather
  than global mutable state. This is an in-memory, single-process task; do
  **not** infer persistence or cross-node guarantees.

For either track:

1. Follow public onboarding material with no private maintainer instructions.
   Record time and any errors from first open through first successful run.
2. Make one independently authored useful change beyond renaming a constant.
   Record the exact source and one repeatable verification command or test.
3. Put the work in a participant-maintained repository **if the participant
   agrees to share one**; otherwise retain an anonymized verification record.
4. Return **at least 24 hours later** to change or fix the program again
   without maintainer intervention. Record whether the return visit happens,
   not just whether it was invited.
5. Ask whether the developer would choose Nulang again for that workload,
   and capture the reason in their words.

A browser-playground session is useful for discovery, but **cannot** complete
the native-actor or HTTP/FS tracks by itself. Record discovery and native
activation as distinct milestones; do not conflate them.

## Task B: durable recovery demonstration (requires validated runtime behavior)

This is a separate technical evaluation, **not a prerequisite for Task A**:

1. Start a native-runtime application with durable state, a long-running unit of work, and a visible state transition.
2. Persist the first transition, terminate the process during subsequent work, and restart from the same storage directory.
3. Demonstrate precisely which transitions are recovered, retried, or rejected; include executable output and storage version.
4. Reproduce a materially comparable workflow using a conventional solution (for example, TypeScript plus a durable-workflow engine).
5. Compare failure semantics, application code, infrastructure, deployment steps, and debugger/observability effort. Separate measured results from design expectations.

Do **not** assert crash consistency, exactly-once side effects, or distributed delivery guarantees on the basis of an example alone. Open runtime correctness issues (including [#1389](https://github.com/nulang-org/nulang/issues/1389)) take precedence over promotional claims.

## Measurement worksheet

Use a **separate copy** of the
[evidence worksheet](ADOPTION_PILOT_REPORT_TEMPLATE.md) for every consenting
participant. It records the environment and exact version, unassisted and
assisted milestones, initial and ≥24-hour follow-up outcomes, exact commands,
reproduction evidence, friction severity, and optional recovery comparisons.

Use explicit `not measured` instead of zero when data is missing. Never
equate downloads, pageviews, GitHub stars, or generated repositories with
independent adoption. Obtain consent before recording or publishing anything
and remove identifiers, credentials, and private data. If consent is
withdrawn, exclude that participant's per-person report from publication.

For aggregate findings, preserve the entire funnel: invited, consented,
attempted, completed independently, completed with help, blocked, declined,
and withdrawn. Avoid reporting a median from an unspecified or selectively
sampled denominator. `Issue #809` is the public tracker; do not post raw
interviews or private transcripts there.

## Decision rules

- **All participants blocked on installation:** fix release, documentation, or editor packaging first.
- **Participants can install but cannot write working examples:** resolve parser/docs mismatch and diagnostic clarity before adding new language features.
- **Participants complete Task A but reject Nulang's value proposition:** narrow or revise the target application category rather than optimizing unrelated benchmarks.
- **Task B demonstrates correctness or reliability gaps:** prioritize the relevant runtime correctness gate; do not use the broken example in marketing.
- **Three complete independent applications or one independently reproducible compelling use case:** update issue #809 with exact evidence, then decide what broader adoption and performance work is justified.

Pilot results belong in [issue #809](https://github.com/nulang-org/nulang/issues/809),
linked to exact example/runtime versions and without identifying participant
information unless explicitly authorized. Only report the gate as passed
after independently reproducible evidence is available; a drafted plan, an
internal demo, or a code-generated sample alone is not external adoption.

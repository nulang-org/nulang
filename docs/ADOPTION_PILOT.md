# Nulang independent-developer adoption pilot

**Status:** Proposed pilot protocol (not evidence of completed recruitment or adoption).  
**Tracker:** [Issue #809](https://github.com/nulang-org/nulang/issues/809)  
**Scope:** Nulang alpha, reference bytecode runtime. This is a developer experience evaluation, **not** a production reliability certification.

## Goal and gate

Recruit **three developers who do not maintain the Nulang compiler** and have not been coached through the implementation. Each person should build or materially extend one useful Nulang application, then independently revisit it for a change or bug fix.

The pilot passes the adoption gate only when **three independent developers finish non-trivial applications**, or when **one production-like reference application** demonstrates a reproducible material advantage over an equivalent conventional stack. Merely starting an example or starring the repository does not satisfy the gate.

## Paths a participant can choose

1. **Discovery:** Run the [Core-only browser playground](https://nulang.org/playground/) without an account or local setup. It cannot run actors, external networking, or durable native runtime behavior.
2. **Local use:** Install a checksummed [tagged release](https://nulang.org/getting-started/installation/) for a supported platform, if a corresponding release artifact is available, then open the [quick start](https://nulang.org/getting-started/quick-start/).
3. **Contributing:** Follow the [source-build guide](https://nulang.org/getting-started/installation/#build-from-source-contributors-and-custom-profiles) only if modifying the compiler or testing an unsupported platform/profile.

Ask each participant to narrate where instructions are confusing. **Do not repair their environment for them until they have recorded the blocker.**

## Task A: first useful program

- Begin with the existing `examples/16_realworld.nula` or `examples/17_actor_fetcher.nula`, but modify it for a genuine use case (new input, error handling, data transformation, or output).
- Run the result locally, add at least one meaningful test or repeatable verification command, and commit the source to an independently maintained repository.
- Record the steps, compiler/runtime version, elapsed setup time, operating system, blockers, and code changes.
- Return at least one day later to extend or repair the project without maintainer intervention.

## Task B: durable recovery demonstration (requires validated runtime behavior)

This is a separate technical evaluation, **not a prerequisite for Task A**:

1. Start a native-runtime application with durable state, a long-running unit of work, and a visible state transition.
2. Persist the first transition, terminate the process during subsequent work, and restart from the same storage directory.
3. Demonstrate precisely which transitions are recovered, retried, or rejected; include executable output and storage version.
4. Reproduce a materially comparable workflow using a conventional solution (for example, TypeScript plus a durable-workflow engine).
5. Compare failure semantics, application code, infrastructure, deployment steps, and debugger/observability effort. Separate measured results from design expectations.

Do **not** assert crash consistency, exactly-once side effects, or distributed delivery guarantees on the basis of an example alone. Open runtime correctness issues (including [#1389](https://github.com/nulang-org/nulang/issues/1389)) take precedence over promotional claims.

## Measurement worksheet

For each participant, capture:

| Field | What to record |
| --- | --- |
| Participant | Anonymous pilot ID and prior language experience |
| Runtime | Nulang release SHA/tag, CLI version, OS, CPU architecture |
| Start | How they discovered Nulang and which entry path they chose |
| Activation | Minutes to first successful program and first self-authored change |
| Completion | Task attempted, repository URL (if consented), and whether independent |
| Friction | Exact confusing instruction, error message, or missing capability |
| Repeat usage | Did they return and modify the program after >=1 day? |
| Outcome | Would they use Nulang for this use case again? Why or why not? |

Use explicit `not measured` instead of zero when data is missing. Do not equate downloads, pageviews, GitHub stars, or generated repositories with independent adoption. Obtain consent before publishing user feedback, recording sessions, or linking participant repositories.

## Decision rules

- **All participants blocked on installation:** fix release, documentation, or editor packaging first.
- **Participants can install but cannot write working examples:** resolve parser/docs mismatch and diagnostic clarity before adding new language features.
- **Participants complete Task A but reject Nulang's value proposition:** narrow or revise the target application category rather than optimizing unrelated benchmarks.
- **Task B demonstrates correctness or reliability gaps:** prioritize the relevant runtime correctness gate; do not use the broken example in marketing.
- **Three complete independent applications or one independently reproducible compelling use case:** update issue #809 with exact evidence, then decide what broader adoption and performance work is justified.

Pilot results belong in issue #809, linked to exact example/runtime versions and without identifying participant information unless explicitly authorized.

# Nulang external-developer pilot: evidence worksheet

Copy this worksheet **once per consenting participant**. Keep the private
participant notes separate from the public, anonymized summary in
[issue #809](https://github.com/nulang-org/nulang/issues/809).

**This is a blank reporting template, not a completed test or adoption claim.**
Use `not measured` for unknown fields; never substitute zero or inferred
results. Do not publish email addresses, real names, IP addresses, access
tokens, private source code, or unredacted logs.

## Participant and environment

| Field | Observation |
| --- | --- |
| Anonymous ID | `P-___` |
| Consent for participation | yes / no / withdrawn |
| Consent to publish anonymous findings | yes / no / withdrawn |
| Previous language / actor-system experience | |
| Operating system and architecture | |
| Entry path | browser Core / tagged native release / source build |
| Nulang release tag and/or exact commit SHA | |
| `nulang --version` output, if available | |
| Native binary / feature flags (if applicable) | |
| Example or application commit SHA | |
| Task track | CLI data transformation / local actor state |
| Assistance requested or supplied by a maintainer | none / describe exact intervention |

Do not begin a publication workflow without explicit consent. A participant
who withdraws consent must not be represented in published per-person
evidence. Count that participant as `withdrawn`, not a technical failure.

## Session 1: unassisted first-use attempt

Record clock times or elapsed minutes. Start when the participant first opens
the [pilot instructions](ADOPTION_PILOT.md); stop when the stated outcome is
observed. Do **not** exclude installation or debugging time.

| Milestone | Minutes since start, or `not measured` | Evidence / notes |
| --- | --- | --- |
| Entry material opened | | |
| Install / browser launch attempted | | |
| First successful example executed | | |
| First independent code change attempted | | |
| First independent code change verified | | |
| Own small application or extension completed | | |

**Chosen task and intended outcome:**

**Exact command(s) attempted (remove credentials):**

```text
# Copy commands here, if consented.
```

**Observed output, exit status, or error (redacted):**

```text
# Include reproducible lines; link to a consensually shared test repo if available.
```

**Reproduction / verification:** Which command proves the newly authored
behavior? State whether you reran it after the change. If the outcome is
not deterministic, record that explicitly.

**Result:** completed independently / completed with assistance /
blocked / abandoned / not measured.

## Session 2: return visit (at least 24 hours later)

| Field | Observation |
| --- | --- |
| Return interval since session 1 | |
| Task attempted without maintainer assistance | |
| Minutes to first meaningful new change | |
| Exact verification command and output | |
| Result | completed / blocked / did not return / not measured |
| Would use Nulang for this workload again? Why? | |

A one-time Hello World does not satisfy the independent-application gate.
If the participant does not return, retain the result as incomplete rather
than retroactively marking success.

## Friction log

Add one row per distinct problem. Keep the participant's own phrasing separate
from the maintainer's interpretation.

| Step | Observed problem and exact evidence | Severity | Maintainer hypothesis | Issue link |
| --- | --- | --- | --- | --- |
| | | blocking / major / minor | unverified / confirmed | |

Severity guidance: **blocking** stops task completion; **major** permits a
workaround but threatens repeat use; **minor** slows progress without changing
the outcome. If a failure is ambiguous, do not classify its root cause as
confirmed.

## Optional: crash/recovery evaluation

Only for participants who deliberately choose the separate durable-recovery
task and can reproduce it with a native runtime. Record storage engine,
persistence directory (redacted), exact failure injection, recovered state,
effect receipt/idempotency behavior, and comparison stack. A successful restart
alone does **not** prove exactly-once external effects or a production
reliability guarantee.

| Claim | Nulang observed evidence | Comparison stack evidence | Not tested / limitation |
| --- | --- | --- | --- |
| Persistence and restart | | | |
| Duplicate-effect handling | | | |
| Stale-writer/activation fencing | | | |
| Deployment and operational burden | | | |
| Source code and external services required | | | |

## Maintainer classification and aggregate reporting

- **Eligible as an independently completed application?** yes / no / unverified
- **Reason and reproducible application evidence:**
- **Specific missing capability, documentation problem, or correctness defect:**
- **Suggested narrowly scoped follow-up issue:**
- **Publication permission confirmed?** yes / no

When reporting across participants, include the complete denominator (invited,
consented, attempted, completed independently, assisted, blocked, declined,
withdrawn) rather than only successes. Report median onboarding times **only
for actually measured samples**, alongside sample count and missing values.
Use the [pilot gate](ADOPTION_PILOT.md) and [semantic stabilization
contract](SEMANTIC_STABILIZATION_CONTRACT.md) to distinguish an early adoption
milestone from a later language-stability decision.

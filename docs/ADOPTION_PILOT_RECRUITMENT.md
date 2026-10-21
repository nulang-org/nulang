# Invite independent developers to evaluate Nulang

**Status:** recruitment material only. No developer is counted as enrolled by
publishing this file. Use it after verifying that the native release and
getting-started instructions are functioning at a recorded tag/commit.

This document accompanies the [pilot protocol](ADOPTION_PILOT.md), the blank
[evidence worksheet](ADOPTION_PILOT_REPORT_TEMPLATE.md), and
[adoption tracker #809](https://github.com/nulang-org/nulang/issues/809).

## Who to invite

The first cohort aims for **three non-maintainer developers** who can follow
public Nulang documentation independently, write a small but useful extension,
and return at least 24 hours later to modify their code again.

Recruit across experience levels instead of only inviting language designers
and actor-system experts. Experience with Rust, Go, Elixir, TypeScript or Python
can help, but Rust is **not required** for evaluating the prebuilt native CLI.
Prior Nulang contributors and people coached through the implementation should
not be counted as independent participants in the initial gate.

Participation is voluntary. Before enrollment, explain how notes will be
collected, what is published, and how consent can be withdrawn. Never require
participants to publish private source code, personal contact details or
screen recordings.

## Short public invitation (copy/adapt)

> **Help test Nulang's first-run developer experience**
>
> Nulang is an alpha programming language exploring typed actors, explicit
> effects, and durable/distributed software. We're looking for independent
> developers to evaluate the actual onboarding experience, not to endorse
> production readiness.
>
> The task: follow the public install guide, choose either a small native
> HTTP/CLI task or an in-memory actor-state task, independently add a useful
> behavior and verify it, then revisit the code for another change at least
> a day later. We especially want to hear about blockers, unclear diagnostics,
> missing libraries, and reasons you would *not* choose Nulang.
>
> See the [pilot instructions](https://github.com/nulang-org/nulang/blob/main/docs/ADOPTION_PILOT.md)
> and [issue #809](https://github.com/nulang-org/nulang/issues/809) for
> public project context. Do not share identifying information, credentials,
> or private debugging logs in public GitHub issues.
>
> Participation is voluntary; we ask permission before recording or sharing
> anonymized results. Nulang is experimental, and this is not a production
> reliability test.

When this file is still in a PR, share the **exact PR branch** instructions
instead of the `main` link above, or wait until the instructions have been
reviewed and merged.

## Recruit, run, and report

1. **Verify the starting point.** Confirm checksummed release artifacts
   actually exist for the chosen OS/architecture, compile/run one native
   example, and record the exact release tag or source commit. If validation
   is blocked by CI or GitHub availability, mark recruitment as **not
   started**; do not claim ready-to-run onboarding.
2. **Invite intentionally.** Reach out to a range of independent developer
   communities and ask volunteers to opt in. Do not post unsolicited
   mass mentions, scrape private emails, or promise compensation that has
   not been approved.
3. **Use one consistent briefing.** Send the public
   [pilot tasks](ADOPTION_PILOT.md) to each participant. Capture which
   task they chose. Avoid hands-on help until their original blocker has been
   recorded. Assisted completions are useful findings, not unassisted success.
4. **Record one worksheet per participant** and schedule the return attempt
   for at least 24 hours later. Keep raw evidence private where necessary.
5. **Report the denominator and outcomes** in #809 only with participant
   permission. Separate invited, declined, consented, attempted, assisted,
   blocked, independently completed, follow-up completed, and withdrawn.
6. **Prioritize reproducible blockers** and defer broad performance or new
   core features unless the participant evidence justifies them.

Three independently finished non-trivial applications satisfy the **initial
adoption pilot's** developer-count criterion. It does not substitute for the
separate broader semantic-stabilization evidence or warrant claims about
production-grade durability, distribution, or exactly-once external effects.

## What changes, and why?

Describe the user-visible behavior or invariant being changed, with linked issue/RFC.
Keep unrelated work out of this PR; identify any stacked dependency and exact base PR.

## Risk and trust boundaries

- Components touched (compiler / VM / JIT / actors / durability / network / registry / FFI / CI / docs):
- Input and trust boundary affected (or "none"):
- New capabilities, filesystem/network access, or credential exposure (or "none"):
- Failure mode if the change is wrong:
- Compatibility surface / stability tier; accepted RFC if required:

## Tests and reproducible evidence

- Exact PR head SHA tested:
- Commands **actually run**, with observed results:
- New or updated regression tests (include malformed/adversarial cases if applicable):
- Differential, conformance, fuzz, or crash/recovery coverage where applicable:
- Checks **not run** and why:
- Benchmark environment / paired baseline results for performance claims (or "not applicable"):

## Security / dependency review

- [ ] No secrets, tokens, personal data, or unsafe workflow event interpolation introduced.
- [ ] New dependencies or GitHub Actions were reviewed for source, permissions and supply-chain risk; or none were added.
- [ ] Any temporary security exception is documented with owner, expiry and removal plan; or none applies.
- [ ] No untrusted code is executed with elevated CI permissions or secrets.

## Merge readiness

- [ ] Change is scoped and reviewable, including relevant changelog / documentation.
- [ ] Required checks completed on the **current head / merge candidate**, not a stale commit.
- [ ] All blocking review comments are addressed and discussions resolved.
- [ ] I have not represented unexecuted tests or unmeasured speedups as verified.

See [Maintainer review policy](../docs/MAINTAINER_REVIEW.md).

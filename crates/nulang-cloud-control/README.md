# nulang-cloud-control

Small control-plane bridge between Nulang's compiled-artifact admission model and the provider-neutral `nulang-capacity` scheduler.

The crate exists to keep both core sides independent:

- `nulang` owns compiled artifact identity, authority, tenant policy, and authoritative admission.
- `nulang-capacity` owns provider normalization, economic ranking, runtime envelopes, isolation eligibility, durable claims, and provider lease primitives.
- `nulang-cloud-control` translates between them and performs the final scheduling/acquisition gate.

## Planning and acquisition flow

```text
validated DeploymentBundle
  -> policy/authority preflight
  -> ExecutionRequirements
  -> capacity-ranked ExecutionTarget list
  -> runtime/isolation eligibility filter
  -> final artifact admission per RuntimeEnvelope
  -> construct immutable audit evidence per eligible target
  -> durable placement claim
  -> idempotent ranked provider lease acquisition
  -> selected admitted runtime target + audit record
  -> persist selected audit record
  -> launch execution
```

The bridge never converts authority into a score. A target either satisfies runtime/isolation constraints or it does not. Economic ordering remains owned by the capacity broker, while the exact artifact and tenant policy remain owned by the admission engine.

Audit evidence is constructed for every eligible target **before any provider side effect**, so malformed evidence cannot strand capacity. The selected audit record is persisted after the provider lease identifies the actual fallback winner and before that capacity is allowed to execute the workload.

## Trust rules

1. Only a `DeploymentBundle` that passed the public parser is accepted.
2. Tenant authority is evaluated before capacity selection.
3. `external_authority` is an authority marker, not a loadable runtime feature.
4. Runtime features and isolation are hard target constraints.
5. Every eligible target is re-evaluated by authoritative artifact admission.
6. Provider+offer identity must map to exactly one runtime target.
7. Provider lease responses must match the exact request; invalid or indeterminate responses retain the durable claim and stop fallback.
8. The selected `DeploymentAdmissionRecord` must be persisted before launching execution.

This crate intentionally uses its own small Cargo workspace so the root language/runtime workspace and `nulang-capacity` do not acquire a dependency cycle or unnecessary coupling.

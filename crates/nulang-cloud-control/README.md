# nulang-cloud-control

Small control-plane bridge between Nulang's compiled-artifact admission model and the provider-neutral `nulang-capacity` scheduler.

The crate exists to keep both core sides independent:

- `nulang` owns compiled artifact identity, authority, tenant policy, and authoritative admission.
- `nulang-capacity` owns provider normalization, economic ranking, runtime envelopes, isolation eligibility, durable claims, provider leases, and fail-closed reconciliation.
- `nulang-cloud-control` translates between them and binds every acquired/reconciled lease back to the exact runtime target and admission evidence allowed to execute it.

## Planning, acquisition, and reconciliation flow

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
      ├─ success -> selected admitted target + audit record
      └─ ambiguous -> keep claim -> reconcile same exact LeaseRequest
             ├─ recovered exact lease -> selected admitted target + audit record
             ├─ pending -> no execution and no fallback
             └─ confirmed absent -> resume after original offer
  -> persist selected audit record
  -> launch execution
```

The bridge never converts authority into a score. A target either satisfies runtime/isolation constraints or it does not. Economic ordering remains owned by the capacity broker, while the exact artifact and tenant policy remain owned by the admission engine.

Audit evidence is constructed for every eligible target **before any provider side effect**, so malformed evidence cannot strand capacity. The selected audit record is persisted after acquisition/reconciliation identifies the actual winner and before that capacity is allowed to execute the workload.

## Trust rules

1. Only a `DeploymentBundle` that passed the public parser is accepted.
2. Tenant authority is evaluated before capacity selection.
3. `external_authority` is an authority marker, not a loadable runtime feature.
4. Runtime features and isolation are hard target constraints.
5. Every eligible target is re-evaluated by authoritative artifact admission.
6. Provider+offer identity must map to exactly one runtime target.
7. Provider lease responses must match the exact request; invalid or indeterminate responses retain the durable claim and stop fallback.
8. Reconciliation uses the exact blocked `LeaseRequest`; pending or invalid recovered state remains non-executable.
9. Only provider-confirmed absence can resume fallback, starting after the reconciled offer with the same job and placement token.
10. Any recovered/resumed lease is rebound to the pre-admitted target and its `DeploymentAdmissionRecord`; raw provider state is never promoted directly to execution.
11. The selected `DeploymentAdmissionRecord` must be persisted before launching execution.

This crate intentionally uses its own small Cargo workspace so the root language/runtime workspace and `nulang-capacity` do not acquire a dependency cycle or unnecessary coupling.

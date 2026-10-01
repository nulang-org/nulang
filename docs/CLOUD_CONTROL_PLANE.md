# Nulang Cloud Control Plane

**Status:** Frozen migration surface. Hosted Cloud placement and reconciliation
authority lives in `dporkka/nulang-cloud` under `nlc-placement`.

The historical implementation in `crates/nulang-cloud-control` is retained
temporarily so existing branches can migrate without a flag day. It is excluded
from the root Nulang workspace and must not receive new Cloud policy, provider,
database, billing, tenancy, or deployment features.

## Ownership boundary

Nulang and Nulang Cloud deliberately solve different classes of problems:

- **Nulang owns computation semantics:** actors, effects, durable transitions,
  timers, messaging/Fabric, runtime identity, capability contracts, and portable
  execution metadata.
- **Nulang Cloud owns resource and product policy:** regions/cells, provider
  inventory, workload placement, node lifecycle, tenant quotas, deployment
  reconciliation, metering, billing, and provider-specific execution policy.

The actor scheduler remains runtime-local. Cloud placement remains hosted
control-plane policy. The language/runtime hot path must not depend on a hosted
Cloud database or scheduler.

## Canonical Cloud authority

The canonical hosted control-plane model is `nlc-placement` in the Nulang
Cloud repository. It owns:

- `Region -> Cell -> Provider` topology;
- resource inventories and reservations;
- placement generations;
- ownership leases/fencing epochs;
- committed placement state;
- deployment admission integration;
- host-agent inventory and runtime endpoint integration.

No second authoritative placement database or reconciliation state machine
should be added to this repository.

## What remains reusable in Nulang

Only cloud-neutral contracts belong here. Reusable pieces should converge on
`nulang-capacity` (or another runtime-neutral crate) and remain independent of:

- PostgreSQL or other hosted control-plane databases;
- provider SDKs;
- tenant/billing concepts;
- deployment APIs;
- region/cell lifecycle policy.

Good candidates are resource quantities, capability sets, deterministic
constraint evaluation, and generic fencing/epoch value types when they are also
needed by the runtime.

## Legacy `nulang-cloud-control` migration

The existing crate contains useful deterministic planning and commit ideas, but
it duplicates authority already implemented by Nulang Cloud. Migration should
preserve safety properties rather than preserve the package boundary.

Required migration sequence:

1. Map reusable cloud-neutral resource/fencing types into `nulang-capacity`.
2. Port missing deterministic placement tests or safety invariants to
   `nlc-placement`.
3. Port reconciliation/outbox behavior needed by the hosted service into Nulang
   Cloud, using its authoritative placement state.
4. Retarget or close stacked PRs that add Cloud-only behavior to
   `nulang-cloud-control`.
5. Delete `crates/nulang-cloud-control` once no active branch depends on it.

Until step 5, the crate is compatibility-only: fixes required to migrate or
preserve correctness are allowed; feature expansion is not.

## Cross-repository architecture rule

Before adding infrastructure to Nulang, ask whether the behavior is a
**computation semantic** or **hosted resource/product policy**.

```text
computation semantic
    -> Nulang runtime / Fabric / capacity-neutral contract

hosted resource or product policy
    -> Nulang Cloud control plane
```

This rule is intended to prevent a second scheduler, broker, workflow engine, or
placement authority from emerging beside an existing implementation.

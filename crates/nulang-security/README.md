# nulang-security

Dependency-light security primitives shared by the Nulang runtime and agent tooling.

This crate owns identity, explicit Unix-second time, and delegation provenance metadata. It deliberately does **not** own runtime capability evaluation, application RBAC/ReBAC/ABAC policy, token formats, cryptographic key management, storage, or network protocols.

## Stable representation boundary

Shared wire-facing primitives use explicit, minimal serde forms:

- principal kinds serialize as stable snake-case values such as `agent` and `workload`;
- `UnixSeconds` serializes as an integer count of whole Unix seconds;
- `DelegationId` and `RevocationDomainId` serialize as validated string identifiers;
- `RevocationEpoch` serializes as an unsigned integer generation;
- `RevocationVersion` carries both the revocation domain and its epoch.

Revocation epochs are intentionally **not globally comparable**. `RevocationVersion::compare_epoch` returns no ordering when the domains differ, so an authorization layer must fail closed instead of comparing unrelated tenant or issuer counters.

The embedding trust domain remains responsible for authenticating principals, assigning delegation IDs, choosing stable revocation-domain IDs, advancing revocation epochs, and choosing any signed-envelope or token format. Keeping those concerns outside this crate prevents identity metadata from becoming an implicit policy or cryptographic protocol.

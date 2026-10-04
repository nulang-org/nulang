# nulang-security

Dependency-light security primitives shared by the Nulang runtime and agent tooling.

This crate owns identity, explicit Unix-second time, and delegation provenance metadata. It deliberately does **not** own runtime capability evaluation, application RBAC/ReBAC/ABAC policy, token formats, cryptographic key management, storage, or network protocols.

## Stable representation boundary

Shared wire-facing primitives use explicit, minimal serde forms:

- principal kinds serialize as stable snake-case values such as `agent` and `workload`;
- `UnixSeconds` serializes as an integer count of whole Unix seconds;
- `DelegationId` serializes as its validated string identifier;
- `RevocationEpoch` serializes as an unsigned integer generation.

The embedding trust domain remains responsible for authenticating principals, assigning delegation IDs, defining the scope of a revocation epoch, and choosing any signed-envelope or token format. Keeping those concerns outside this crate prevents identity metadata from becoming an implicit policy or cryptographic protocol.

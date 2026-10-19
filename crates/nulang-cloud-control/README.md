# Legacy Nulang Cloud control-plane crate

This crate is a **frozen migration surface**.

The authoritative hosted Nulang Cloud placement/control-plane implementation is
`nlc-placement` in the Nulang Cloud repository. This crate is excluded from
the root Nulang workspace so ordinary language/runtime builds do not inherit a
second Cloud control plane or its persistence dependencies.

Do not add new provider, database, tenancy, billing, deployment, or Cloud
scheduling features here.

During migration, changes are limited to:

- correctness fixes needed to preserve existing safety guarantees;
- extracting cloud-neutral resource/capability/fencing contracts into
  `nulang-capacity`;
- porting tests/invariants to the Nulang Cloud authority;
- compatibility changes needed to retire this crate.

See `docs/CLOUD_CONTROL_PLANE.md` for the ownership boundary and deletion
sequence.

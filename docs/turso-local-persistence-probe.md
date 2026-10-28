# Experimental local Turso persistence bridge (non-release)

**Do not use with production databases.** Feature `turso-local-probe` is an
explicit, non-default/non-release opt-in that is not included by `full` or
`release-cli`. It makes no remote connections or automatic backend selection.

The `TursoLocalProbeStore` implements `PersistenceStore` for experiments on
**already-initialized disposable SQLite files**. `LibsqlStore::new` must create
the fixture beforehand. `TursoLocalProbeStore::open_existing` refuses missing,
non-SQLite, symlink and in-memory paths, but **does not prove the file is
non-production**. The caller is responsible for using a disposable file.

For atomic `commit_transition`, the adapter uses the tested Turso local writer
from the standalone compatibility harness (#2301/#2380). It checks the live
connection's `PRAGMA synchronous=FULL`, canonical transition digest,
activation epoch, predecessor, tail CAS and exact-once retry before COMMIT.
An error rolls back or returns an unverified-commit error rather than reporting
success. All other `PersistenceStore` methods (including legacy individual
writes and recovery reads) remain delegated to the existing libSQL backend.
This mixed-driver design is **not a complete Turso migration** and retains the
legacy libSQL and Hyper 0.14 graph in `Cargo.lock`.

Manual disposable-file test (never uses remote credentials):

```sh
cargo test --locked --no-default-features --features turso-local-probe \
  --test turso_local_store_optin
```

The existing separate `tools/turso-storage-compat/` harness remains the more
comprehensive baseline for two-process races, SIGKILL, six-group durability and
cross-driver schema/authority identity. These tests do **not** establish real
power-loss durability, multi-host consensus, remote Turso semantics, complete
application-level recovery compatibility, safe in-place upgrades or production
permission. Before promotion: prove the full `PersistenceStore` contract with
Turso-native reads/writes, independent schema bootstrapping, fault-injected
recovery, real disposable authenticated Turso remote transactions, rollback,
all release-feature profiles, and an unsuppressed security audit with no
legacy `h2 0.3` graph. See issue #2293.

The path-scoped `.woodpecker/turso-local-probe.yml` additionally runs the
feature-gated test and strict Clippy. Verify an actual exact-head Woodpecker
pipeline before claiming its results; root GitHub security checks remain
independent and currently fail for inherited RustSec advisories.

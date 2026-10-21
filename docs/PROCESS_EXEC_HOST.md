# Trusted host process execution (experimental)

The Rust module `nulang::process_exec` adds a **deny-by-default,
argv-based, bounded process boundary** for trusted embedders.

This is an experimental host API, **not** a Nulang language builtin or an
actor-backed effect. It does not change the semantics of `Process.run`.
In particular:

- Standalone `perform Process.run(command)` still invokes `/bin/sh -c`
  with ambient host permissions. Treat it as trusted-only.
- Actor-backed production `Process.run` is still not dispatched pending a
  real operating-system process sandbox.
- Do not call the synchronous `execute` API from a cooperative actor turn or
  directly during durable replay. A separate bounded worker and idempotency
  boundary will be required for that use case.

## Example

```rust
use nulang::process_exec::{execute, ExecLimits, ProcessPolicy, ProcessSpec};

let spec = ProcessSpec::new(
    "/usr/bin/git",
    "/srv/checkout",
    ["status", "--short"],
)?;
let mut policy = ProcessPolicy::new();
policy.allow_exact(spec.clone()); // Only trusted host code grants authority.

let output = execute(&policy, &spec, ExecLimits::default())?;
if !output.status.success() {
    eprintln!("command exited with {:?}", output.status.code());
}
println!("{}", String::from_utf8_lossy(&output.stdout));
```

The exact executable path, argument values, and working directory are matched
as one identity. Arguments never pass through an implicit shell. Execution
clears the inherited environment, closes stdin, and captures stdout and stderr
as separate bytes. A nonzero exit is returned in `ExecOutput.status` instead
of being conflated with spawn failure.

Default capture limits are **30 seconds** and **1 MiB total stdout + stderr**;
custom limits are capped at 64 MiB. Exceeding output size or deadline kills
and reaps the direct child. On Unix, the runner also makes the child a process
group leader and attempts to kill that group if the leader is still alive.

## Security boundaries and non-goals

An exact command grant is **authorization**, not process sandboxing. The
authorized executable retains the invoking OS user's privileges and can
access files and the network and start child processes. Binary replacement,
symbolic links, dynamic libraries, filesystem races, and commands that accept
script/configuration inputs can all undermine a naive host policy. Grant only
immutable, trusted executables and fixed argument vectors, in a separate
restricted OS environment if arbitrary code is possible.

This initial API is synchronous, has no stdin support, does not inherit host
environment variables, and is not a pipeline or `Stream[T]` implementation.
On some platforms, a subprocess can escape direct-child termination by
spawning independent descendants. It should not be advertised as a complete
process-tree sandbox or resource-isolation solution. It also does not guarantee
CPU, memory, disk, or network resource limits.

Follow-up work should add independent process-worker isolation, cancellation,
structured environment policy, language-level typed outcome values, and
eventually bounded streaming stdout/stderr. Integrating it into actor-backed
or durable contexts requires sandbox enforcement and replay safety first.

## Validation

```bash
cargo test --no-default-features --test process_exec_boundary
cargo fmt --all -- --check
cargo check --no-default-features
```

Contract tests cover exact grant denial, literal argv handling, inherited
environment isolation, separate output channels, nonzero exit, deadline,
capture limit, and invalid specifications.

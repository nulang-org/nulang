# Security Policy

Nulang includes a compiler, bytecode VM, JIT/native code generation, durable
actor runtime, networking, package-management, FFI, and optional database/AI
integrations. Security reports in those areas are treated as product security
issues, especially when they cross an intended trust boundary.

## Supported versions

Security fixes are made on `main` and, when practical, on the most recent
tagged release. Older experimental releases should be assumed unsupported
unless a release note explicitly says otherwise.

## Reporting a vulnerability

Do **not** publish vulnerability details in a normal GitHub issue or discussion.

Use GitHub's private vulnerability reporting / security-advisory flow for this
repository when the "Report a vulnerability" action is available. Include:

- affected commit or release;
- impacted feature flags / backend / operating system;
- minimal reproduction or proof of concept;
- expected versus observed security boundary;
- whether exploitation requires trusted local code, untrusted Nulang source,
  network access, package-registry access, or host FFI access.

If the private reporting action is unavailable, open a minimal public issue
titled **Security contact request** with no vulnerability details. A maintainer
can then establish a private channel before technical details are shared.

Please avoid public disclosure until a fix or coordinated disclosure plan is
available.

## High-priority security surfaces

Reports are particularly useful for:

- VM/JIT/AOT memory-safety or type-confusion defects;
- bytecode / `.nbc` validation bypasses;
- actor isolation, authority/capability, or cross-node authentication bypasses;
- package integrity, registry authentication, or source-hash verification bugs;
- TLS/certificate-validation failures;
- FFI sandbox/allowlist escapes;
- persistence corruption that can violate durable execution guarantees.

## Trust-boundary notes

The default FFI surface is powerful host integration, not a sandbox for
untrusted native libraries. Likewise, Experimental features do not gain a
security guarantee merely because they compile or have conformance coverage.
See `docs/threat-model.md` for the current threat model and feature-specific
limits.

## Dependency advisories

CI runs `cargo audit`, but `.cargo/audit.toml` contains narrowly documented
temporary suppressions for advisories that are blocked on upstream dependency
upgrades or whose vulnerable APIs are not used by the current implementation.
Those suppressions are accepted risks, not a claim that the dependency graph is
advisory-free. Release decisions should review that file and the relevant
reachable feature paths.

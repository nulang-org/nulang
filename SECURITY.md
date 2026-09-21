# Security Policy

## Reporting a vulnerability

Please report suspected security vulnerabilities privately to **security@nulang.cloud**.

Include, when possible:

- the affected Nulang version or commit
- the affected component or feature
- reproduction steps or a minimal proof of concept
- the security impact and required attacker access
- any suggested mitigation

Do not open a public GitHub issue for an undisclosed vulnerability that could put users at risk.

## Scope

Security reports are especially useful for issues involving:

- VM, JIT, AOT, or WebAssembly memory safety
- sandbox or capability bypasses
- actor-authority escalation
- distributed transport authentication or integrity
- package-manager and registry supply-chain attacks
- FFI boundary violations
- persistence or deserialization bugs that cross trust boundaries
- denial-of-service flaws reachable from untrusted input

The standalone runtime is intentionally capable of trusted local host access unless
`--sandboxed` is used. Reports that demonstrate a way to bypass the documented
`--sandboxed` restrictions are in scope.

## Coordinated disclosure

Please allow time to investigate and prepare a fix before public disclosure. We
will coordinate disclosure timing with reporters when a vulnerability is
confirmed.

## Security defaults

For untrusted programs, use `nulang --sandboxed`. Distributed nodes require
mutual TLS by default; `--plaintext` is an explicit insecure development
opt-out.

Keep Nulang and its Rust dependencies current and review RustSec advisories
before production deployment.

# nulang-security

Dependency-light security primitives shared by the Nulang runtime and agent tooling.

This crate owns identity, explicit Unix-second time, and delegation provenance metadata. It deliberately does **not** own runtime capability evaluation, application RBAC/ReBAC/ABAC policy, token formats, cryptographic key management, storage, or network protocols.

#!/usr/bin/env python3
"""Fail closed if the old libSQL remote/TLS stack enters the root dependency graph."""
import pathlib
import tomllib

root = pathlib.Path(__file__).resolve().parents[2]
manifest = tomllib.loads((root / "Cargo.toml").read_text())
features = manifest["dependencies"]["libsql"]["features"]
assert features == ["core"], f"libsql must remain local-only, got {features!r}"
assert manifest["dependencies"]["libsql"]["default-features"] is False
packages = tomllib.loads((root / "Cargo.lock").read_text())["package"]
for name, prefix in (
    ("rustls-webpki", "0.102."),
    ("hyper-rustls", "0.25."),
    ("rustls", "0.22."),
    ("tokio-rustls", "0.25."),
):
    versions = [p["version"] for p in packages if p["name"] == name]
    assert not any(v.startswith(prefix) for v in versions), (
        f"legacy vulnerable TLS dependency reintroduced: {name} {versions}"
    )
source = (root / "src/runtime/persistence.rs").read_text()
assert "libsql::Builder::new_remote" not in source, "legacy libSQL remote API still reachable"
print("PASS: root libSQL storage is local-only without vulnerable remote TLS stack")

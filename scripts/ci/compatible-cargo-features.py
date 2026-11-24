#!/usr/bin/env python3
"""Select every compatible Cargo feature without disabling untrusted-runner isolation.

Cargo --all-features includes the intentionally incompatible hosted bytecode
profile. Enumerate ALL other root-crate features from metadata, not a manually
maintained subset. New features join this matrix automatically.
"""
import json
import subprocess


def cargo_features():
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--offline",
         "--format-version", "1"],
        check=True, capture_output=True, text=True,
    )
    return next(pkg["features"] for pkg in json.loads(result.stdout)["packages"]
                if pkg["name"] == "nulang")


def compatible_features(features):
    if "untrusted-runner" not in features:
        raise ValueError("missing untrusted-runner Cargo feature; refuse incomplete CI matrix")

    # An alias to the isolated profile would make the combined matrix invalid.
    def reaches_untrusted(name, visited):
        if name == "untrusted-runner":
            return True
        if name in visited:
            return False
        visited.add(name)
        return any(
            reaches_untrusted(child, visited)
            for child in features.get(name, [])
            if child in features
        )

    selected = sorted(set(features) - {"untrusted-runner"})
    for name in selected:
        if reaches_untrusted(name, set()):
            raise ValueError(f"Cargo feature {name} enables untrusted-runner")
    return ",".join(selected)


if __name__ == "__main__":
    print(compatible_features(cargo_features()))

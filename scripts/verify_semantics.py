#!/usr/bin/env python3
"""Validate Nulang's semantic invariant registry and backend conformance matrix."""
from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from typing import Any

INVARIANT_ID = re.compile(r"^[A-Z]+(?:-[A-Z]+)*-\\d{3}$")
INVARIANT_STATUSES = {"enforced", "partial", "target"}
BACKEND_STATUSES = {"primary", "enabled", "experimental"}
SUPPORT_STATUSES = {"reference", "verified", "partial", "unsupported", "not-applicable"}


def _load_json(path: pathlib.Path, errors: list[str]) -> dict[str, Any] | None:
    try:
        with path.open("r", encoding="utf-8") as fh:
            value = json.load(fh)
    except FileNotFoundError:
        errors.append(f"missing required file: {path}")
        return None
    except json.JSONDecodeError as exc:
        errors.append(f"invalid JSON in {path}: {exc}")
        return None
    if not isinstance(value, dict):
        errors.append(f"top-level JSON value must be an object: {path}")
        return None
    return value


def _evidence_path(entry: str) -> str:
    return entry.split("::", 1)[0]


def validate(root: pathlib.Path) -> list[str]:
    errors: list[str] = []
    inv_path = root / "spec/invariants/v0alpha1.json"
    conf_path = root / "spec/backend_conformance/v0alpha1.json"
    inv_doc = _load_json(inv_path, errors)
    conf_doc = _load_json(conf_path, errors)
    if inv_doc is None or conf_doc is None:
        return errors

    if inv_doc.get("schema_version") != 1:
        errors.append("invariant registry schema_version must be 1")
    if conf_doc.get("schema_version") != 1:
        errors.append("backend conformance schema_version must be 1")

    invariants = inv_doc.get("invariants")
    if not isinstance(invariants, list) or not invariants:
        errors.append("invariant registry must contain a non-empty 'invariants' array")
        invariants = []

    invariant_ids: set[str] = set()
    for index, item in enumerate(invariants):
        where = f"invariants[{index}]"
        if not isinstance(item, dict):
            errors.append(f"{where} must be an object")
            continue
        inv_id = item.get("id")
        if not isinstance(inv_id, str) or not INVARIANT_ID.fullmatch(inv_id):
            errors.append(f"{where}.id is invalid: {inv_id!r}")
        elif inv_id in invariant_ids:
            errors.append(f"duplicate invariant id: {inv_id}")
        else:
            invariant_ids.add(inv_id)
        if item.get("status") not in INVARIANT_STATUSES:
            errors.append(f"{where} has invalid invariant status: {item.get('status')!r}")
        for field in ("domain", "summary"):
            if not isinstance(item.get(field), str) or not item[field].strip():
                errors.append(f"{where}.{field} must be a non-empty string")
        evidence = item.get("evidence")
        if not isinstance(evidence, list) or not evidence:
            errors.append(f"{where}.evidence must be a non-empty array")
        else:
            for evidence_entry in evidence:
                if not isinstance(evidence_entry, str) or not evidence_entry.strip():
                    errors.append(f"{where}.evidence entries must be non-empty strings")
                    continue
                path = root / _evidence_path(evidence_entry)
                if not path.exists():
                    errors.append(f"{where} evidence path does not exist: {evidence_entry}")

    backends = conf_doc.get("backends")
    if not isinstance(backends, list) or not backends:
        errors.append("backend conformance must contain a non-empty 'backends' array")
        backends = []

    backend_ids: set[str] = set()
    for index, backend in enumerate(backends):
        where = f"backends[{index}]"
        if not isinstance(backend, dict):
            errors.append(f"{where} must be an object")
            continue
        backend_id = backend.get("id")
        if not isinstance(backend_id, str) or not backend_id.strip():
            errors.append(f"{where}.id must be a non-empty string")
        elif backend_id in backend_ids:
            errors.append(f"duplicate backend id: {backend_id}")
        else:
            backend_ids.add(backend_id)
        if backend.get("status") not in BACKEND_STATUSES:
            errors.append(f"{where} has invalid backend status: {backend.get('status')!r}")
        if not isinstance(backend.get("role"), str) or not backend["role"].strip():
            errors.append(f"{where}.role must be a non-empty string")

    reference_backend = conf_doc.get("reference_backend")
    if reference_backend not in backend_ids:
        errors.append(f"reference_backend {reference_backend!r} is not a declared backend")

    features = conf_doc.get("features")
    if not isinstance(features, list) or not features:
        errors.append("backend conformance must contain a non-empty 'features' array")
        features = []

    feature_ids: set[str] = set()
    for index, feature in enumerate(features):
        where = f"features[{index}]"
        if not isinstance(feature, dict):
            errors.append(f"{where} must be an object")
            continue
        feature_id = feature.get("id")
        if not isinstance(feature_id, str) or not feature_id.strip():
            errors.append(f"{where}.id must be a non-empty string")
        elif feature_id in feature_ids:
            errors.append(f"duplicate feature id: {feature_id}")
        else:
            feature_ids.add(feature_id)
        invariant = feature.get("invariant")
        if invariant not in invariant_ids:
            errors.append(f"{where} references unknown invariant: {invariant!r}")
        support = feature.get("support")
        if not isinstance(support, dict):
            errors.append(f"{where}.support must be an object")
            continue
        missing = backend_ids - set(support)
        if missing:
            errors.append(f"{where}.support omits backends: {', '.join(sorted(missing))}")
        for backend_id, status in support.items():
            if backend_id not in backend_ids:
                errors.append(f"{where}.support references unknown backend: {backend_id}")
            if status not in SUPPORT_STATUSES:
                errors.append(
                    f"{where}.support[{backend_id!r}] has invalid support status: {status!r}"
                )
        if reference_backend in support and support.get(reference_backend) != "reference":
            errors.append(
                f"{where} must mark reference backend {reference_backend!r} as 'reference'"
            )

    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[1],
    )
    args = parser.parse_args(argv)
    errors = validate(args.root.resolve())
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1
    print("semantic invariant registry and backend conformance matrix are valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

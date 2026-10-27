#!/usr/bin/env python3
"""Read-only, fail-closed GitHub main-branch ruleset audit (issue #1652)."""
import argparse
from fnmatch import fnmatchcase
import json
import os
import sys
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

REPO = "nulang-org/nulang"
RULESET_ID = 25012292
GITHUB_ACTIONS_APP_ID = 15368
REQUIRED_CONTEXTS = (
    "Build & Test",
    "Build & Test (wasm-backend feature)",
    "Build without optional features",
    "Release Build",
    "Format, Clippy & Audit",
    "Docs site & browser playground",
    "Validate PR title",
)
# This check is not emitted for Python-only PRs: codeql.yml has Rust/Cargo
# path filters. Requiring it globally would permanently block such PRs.
PATH_FILTERED_CONTEXTS = {"Analyze (Rust) (rust)"}
SKIPPED_CONTEXTS = {
    "Publish Release", "Changelog updated?", "Benchmarks (regression-gated)"
}


def validate(ruleset):
    """Return policy violations, never merge permission."""
    if not isinstance(ruleset, dict):
        return ["ruleset is not a JSON object"]
    issues = []
    if ruleset.get("enforcement") != "active":
        issues.append("ruleset must be active")
    if ruleset.get("target") != "branch":
        issues.append("ruleset must target branches")
    conditions = ruleset.get("conditions")
    conditions = conditions if isinstance(conditions, dict) else {}
    refs = conditions.get("ref_name")
    refs = refs if isinstance(refs, dict) else {}
    includes = refs.get("include")
    # ~ALL is valid and *stronger* than main-only; do not force admins to
    # weaken existing protection of other branches merely to fix main.
    if not isinstance(includes, list) or not any(
        ref in ("refs/heads/main", "~DEFAULT_BRANCH", "~ALL") for ref in includes
    ):
        issues.append("ruleset must include main, ~DEFAULT_BRANCH or ~ALL")
    excludes = refs.get("exclude")
    # GitHub branch patterns may include globs. Reject any pattern that
    # could exclude main; Python fnmatch is conservative for slash-spanning
    # wildcards, so ambiguous patterns cannot silently waive protection.
    if not isinstance(excludes, list) or any(
        not isinstance(ref, str)
        or ref in ("~DEFAULT_BRANCH", "~ALL")
        or fnmatchcase("refs/heads/main", ref)
        for ref in excludes
    ):
        issues.append("ruleset must not exclude main or default branch")
    raw = ruleset.get("rules")
    rules = {r["type"]: r for r in (raw if isinstance(raw, list) else [])
             if isinstance(r, dict) and isinstance(r.get("type"), str)}
    for name in ("deletion", "non_fast_forward"):
        if name not in rules:
            issues.append(f"missing {name} protection")
    review = rules.get("pull_request", {}).get("parameters", {})
    review = review if isinstance(review, dict) else {}
    count = review.get("required_approving_review_count")
    if not isinstance(count, int) or count < 1:
        issues.append("independent approving review required")
    for flag in ("require_last_push_approval", "dismiss_stale_reviews_on_push",
                 "required_review_thread_resolution"):
        if review.get(flag) is not True:
            issues.append(f"review protection missing {flag}")
    check_rule = rules.get("required_status_checks")
    if not isinstance(check_rule, dict):
        issues.append("missing required status checks rule")
    else:
        params = check_rule.get("parameters")
        params = params if isinstance(params, dict) else {}
        if params.get("strict_required_status_checks_policy") is not True:
            issues.append("required status checks need strict current-head validation")
        raw_checks = params.get("required_status_checks")
        checks = {c["context"]: c.get("integration_id")
                  for c in (raw_checks if isinstance(raw_checks, list) else [])
                  if isinstance(c, dict) and isinstance(c.get("context"), str)}
        for context in REQUIRED_CONTEXTS:
            if context not in checks:
                issues.append(f"missing required check: {context}")
            elif checks[context] != GITHUB_ACTIONS_APP_ID:
                issues.append(f"required check has untrusted provider: {context}")
        for context in sorted(SKIPPED_CONTEXTS.intersection(checks)):
            issues.append(f"skipped workflow must not be a required check: {context}")
        for context in sorted(PATH_FILTERED_CONTEXTS.intersection(checks)):
            issues.append(f"path-filtered check must not be globally required: {context}")
    actors = ruleset.get("bypass_actors")
    if not isinstance(actors, list):
        issues.append("bypass actor list is not inspectable")
        actors = []
    if any(isinstance(a, dict) and a.get("bypass_mode") == "always"
           and a.get("actor_type") in ("RepositoryRole", "Team") for a in actors):
        issues.append("broad role/team bypass requires admin review and restriction")
    return issues


def read_ruleset(path):
    if path:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    headers = {"Accept": "application/vnd.github+json",
               "X-GitHub-Api-Version": "2022-11-28",
               "User-Agent": "nulang-read-only-ruleset-audit"}
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = "Bearer " + token
    url = f"https://api.github.com/repos/{REPO}/rulesets/{RULESET_ID}"
    with urlopen(Request(url, headers=headers, method="GET"), timeout=15) as response:
        return json.load(response)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", help="audit locally saved ruleset JSON; no network")
    args = parser.parse_args(argv)
    try:
        ruleset = read_ruleset(args.file)
    except (OSError, ValueError, HTTPError, URLError) as error:
        reason = f"HTTP {error.code}" if isinstance(error, HTTPError) else type(error).__name__
        print(f"FAIL: could not read ruleset ({reason})", file=sys.stderr)
        return 2
    issues = validate(ruleset)
    if issues:
        print(f"FAIL: main branch ruleset has {len(issues)} issue(s):")
        for issue in issues:
            print(" - " + issue)
        return 1
    print("PASS: required check sources and branch/review ruleset settings verified.")
    print("NOTE: independent review and non-bypass negative merge tests still required.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

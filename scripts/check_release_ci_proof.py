#!/usr/bin/env python3
"""Block publication unless the tagged commit has complete, successful main CI.

Checks GitHub's recorded, executable job results, not badges or commit status
contexts. A green workflow with skipped/missing critical jobs is insufficient.
This is a release provenance gate, NOT a durable-runtime certification.
"""

import argparse
import json
import os
import re
import sys
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen


REQUIRED_CI_JOBS = (
    "Build & Test",
    "Format, Clippy & Audit",
    "Build without optional features",
    "Build & Test (wasm-backend feature)",
    "Release Build",
    "Docs site & browser playground",
    "Nula Package Test",
    "Lean Formalization",
)


class ProofError(Exception):
    """The release does not have sufficient executable validation evidence."""


def require_complete_page(payload, key):
    entries = payload.get(key)
    total = payload.get("total_count")
    if not isinstance(entries, list) or not isinstance(total, int) or total < 0:
        raise ProofError(f"invalid GitHub {key} response")
    if len(entries) != total:
        raise ProofError(f"incomplete GitHub {key}: got {len(entries)} of {total}")
    return entries


def choose_latest_main_push_run(runs, sha):
    candidates = [
        run for run in runs
        if run.get("head_sha") == sha
        and run.get("event") == "push"
        and run.get("head_branch") == "main"
    ]
    if not candidates:
        raise ProofError(f"no CI main-push run for exact commit {sha}")
    # Never fall back to an older successful run if the latest run failed or
    # has not executed. GitHub updates reruns on the same workflow-run record.
    return max(candidates, key=lambda run: (run.get("created_at") or "", run.get("id") or 0))


def validate_exact_head_ci(run, jobs, sha):
    if (run.get("head_sha") != sha or run.get("head_branch") != "main"
            or run.get("event") != "push"):
        raise ProofError("workflow run does not match tagged main-branch commit")
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        raise ProofError(f"exact-head CI has not succeeded: {run.get('status')}/{run.get('conclusion')}")
    if not jobs:
        raise ProofError("CI workflow recorded zero jobs; no validation was executed")

    by_name = {}
    for job in jobs:
        name = job.get("name")
        if not isinstance(name, str) or name in by_name:
            raise ProofError(f"invalid or duplicate CI job name: {name!r}")
        by_name[name] = job

    for name in REQUIRED_CI_JOBS:
        job = by_name.get(name)
        if job is None:
            raise ProofError(f"required executable CI job missing: {name}")
        if job.get("status") != "completed" or job.get("conclusion") != "success":
            raise ProofError(
                f"required CI job {name!r} not successful: "
                f"{job.get('status')}/{job.get('conclusion')}"
            )


def github_json(api_path, token):
    request = Request(
        f"https://api.github.com{api_path}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "nulang-release-ci-proof",
        },
    )
    with urlopen(request, timeout=20) as response:
        return json.load(response)


def check_release(repo, sha, token):
    if not re.fullmatch(r"[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+", repo):
        raise ProofError("repository must be an owner/name identifier")
    if not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
        raise ProofError("tag must resolve to a full 40-character commit SHA")
    if not token:
        raise ProofError("GITHUB_TOKEN is required to verify Actions results")

    base = f"/repos/{repo}"
    query = urlencode({"head_sha": sha, "branch": "main", "event": "push", "per_page": 100})
    runs_payload = github_json(f"{base}/actions/workflows/ci.yml/runs?{query}", token)
    runs = require_complete_page(runs_payload, "workflow_runs")
    run = choose_latest_main_push_run(runs, sha)

    # filter=latest prevents old attempts' jobs from masquerading as current.
    job_query = urlencode({"filter": "latest", "per_page": 100})
    jobs_payload = github_json(f"{base}/actions/runs/{run['id']}/jobs?{job_query}", token)
    jobs = require_complete_page(jobs_payload, "jobs")
    validate_exact_head_ci(run, jobs, sha)
    return run["id"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="GitHub owner/name")
    parser.add_argument("--sha", required=True, help="resolved release commit SHA")
    args = parser.parse_args()
    try:
        run_id = check_release(args.repo, args.sha, os.environ.get("GITHUB_TOKEN"))
    except (ProofError, HTTPError, URLError, OSError, ValueError, KeyError) as exc:
        print(f"release CI proof FAILED: {exc}", file=sys.stderr)
        return 1
    print(f"release CI proof passed: {args.repo}@{args.sha} CI run {run_id}; "
          f"{len(REQUIRED_CI_JOBS)} mandatory jobs completed successfully")
    return 0


if __name__ == "__main__":
    sys.exit(main())

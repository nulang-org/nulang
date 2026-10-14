"""Release evidence must prove a successful main-push CI run for the tag commit."""
import importlib.util
from pathlib import Path
import unittest
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "check_release_ci_proof.py"
spec = importlib.util.spec_from_file_location("release_ci_proof", MODULE_PATH)
proof = importlib.util.module_from_spec(spec)
spec.loader.exec_module(proof)
SHA = "a" * 40


def valid_run(**overrides):
    run = {
        "id": 1234,
        "head_sha": SHA,
        "head_branch": "main",
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "created_at": "2026-10-14T15:00:00Z",
    }
    run.update(overrides)
    return run


def valid_jobs():
    return [
        {"name": name, "status": "completed", "conclusion": "success"}
        for name in proof.REQUIRED_CI_JOBS
    ]


class ExactHeadReleaseProofTests(unittest.TestCase):
    def test_valid_exact_head_run_and_executed_jobs_pass(self):
        run = proof.choose_latest_main_push_run([valid_run()], SHA)
        proof.validate_exact_head_ci(run, valid_jobs(), SHA)

    def test_wrong_sha_or_non_main_or_non_push_cannot_substitute(self):
        for change in (
            {"head_sha": "b" * 40},
            {"head_branch": "feature/fake"},
            {"event": "pull_request"},
        ):
            with self.subTest(change=change):
                with self.assertRaises(proof.ProofError):
                    proof.choose_latest_main_push_run([valid_run(**change)], SHA)

    def test_latest_run_queued_must_not_use_older_success(self):
        older = valid_run(id=1, created_at="2026-10-13T10:00:00Z")
        newest = valid_run(id=2, status="queued", conclusion=None)
        with self.assertRaises(proof.ProofError):
            proof.validate_exact_head_ci(
                proof.choose_latest_main_push_run([older, newest], SHA),
                valid_jobs(),
                SHA,
            )

    def test_missing_or_zero_jobs_are_not_green(self):
        for jobs in ([], valid_jobs()[:-1]):
            with self.subTest(count=len(jobs)), self.assertRaises(proof.ProofError):
                proof.validate_exact_head_ci(valid_run(), jobs, SHA)

    def test_skipped_queued_failed_or_cancelled_required_job_fails(self):
        for status, conclusion in (
            ("completed", "skipped"),
            ("queued", None),
            ("completed", "failure"),
            ("completed", "cancelled"),
        ):
            with self.subTest(status=status, conclusion=conclusion):
                jobs = valid_jobs()
                jobs[0] = {"name": jobs[0]["name"], "status": status, "conclusion": conclusion}
                with self.assertRaises(proof.ProofError):
                    proof.validate_exact_head_ci(valid_run(), jobs, SHA)

    def test_duplicate_job_name_fails_closed(self):
        jobs = valid_jobs()
        with self.assertRaises(proof.ProofError):
            proof.validate_exact_head_ci(valid_run(), jobs + [jobs[0]], SHA)

    def test_workflow_success_must_be_terminal(self):
        for run in (
            valid_run(status="in_progress", conclusion=None),
            valid_run(conclusion="failure"),
        ):
            with self.subTest(run=run), self.assertRaises(proof.ProofError):
                proof.validate_exact_head_ci(run, valid_jobs(), SHA)

    def test_incomplete_github_pagination_fails_closed(self):
        with self.assertRaises(proof.ProofError):
            proof.require_complete_page({"total_count": 101, "jobs": valid_jobs()}, "jobs")

    def test_missing_run_fails_closed(self):
        with self.assertRaises(proof.ProofError):
            proof.choose_latest_main_push_run([], SHA)

    def test_github_verification_queries_exact_main_sha_and_latest_jobs(self):
        requests = []

        def fake_github_json(path, token):
            requests.append((path, token))
            if "/actions/workflows/ci.yml/runs?" in path:
                return {"total_count": 1, "workflow_runs": [valid_run()]}
            return {"total_count": len(valid_jobs()), "jobs": valid_jobs()}

        with mock.patch.object(proof, "github_json", side_effect=fake_github_json):
            self.assertEqual(proof.check_release("nulang-org/nulang", SHA, "fixture-token"), 1234)

        self.assertIn("head_sha=" + SHA, requests[0][0])
        self.assertIn("branch=main", requests[0][0])
        self.assertIn("event=push", requests[0][0])
        self.assertIn("/actions/runs/1234/jobs?", requests[1][0])
        self.assertIn("filter=latest", requests[1][0])
        self.assertEqual([token for _, token in requests], ["fixture-token"] * 2)

    def test_invalid_identifiers_or_missing_token_fail_before_api(self):
        for repo, sha, token in (
            ("not-a-repo", SHA, "token"),
            ("nulang-org/nulang", "not-a-sha", "token"),
            ("nulang-org/nulang", SHA, None),
        ):
            with self.subTest(repo=repo, sha=sha, token=token):
                with mock.patch.object(proof, "github_json") as api:
                    with self.assertRaises(proof.ProofError):
                        proof.check_release(repo, sha, token)
                    api.assert_not_called()


if __name__ == "__main__":
    unittest.main()

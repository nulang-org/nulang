"""Contract tests for the read-only, fail-closed GitHub main ruleset audit."""
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "admin" / "audit_main_ruleset.py"
spec = importlib.util.spec_from_file_location("audit_main_ruleset", SCRIPT)
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


def safe_ruleset():
    return {
        "enforcement": "active",
        "target": "branch",
        "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
        "rules": [
            {"type": "deletion"},
            {"type": "non_fast_forward"},
            {"type": "pull_request", "parameters": {
                "required_approving_review_count": 1,
                "require_last_push_approval": True,
                "dismiss_stale_reviews_on_push": True,
                "required_review_thread_resolution": True,
            }},
            {"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": True,
                "required_status_checks": [
                    {"context": name, "integration_id": 15368}
                    for name in audit.REQUIRED_CONTEXTS
                ],
            }},
        ],
        "bypass_actors": [],
    }


class MainRulesetAuditTests(unittest.TestCase):
    def test_complete_policy_is_accepted(self):
        self.assertEqual(audit.validate(safe_ruleset()), [])

    def test_scope_missing_main_is_rejected(self):
        rule = safe_ruleset()
        rule["conditions"]["ref_name"]["include"] = ["refs/heads/develop"]
        self.assertTrue(any("main" in s for s in audit.validate(rule)))

    def test_all_branches_scope_protects_main_without_weakening_other_branches(self):
        rule = safe_ruleset()
        rule["conditions"]["ref_name"]["include"] = ["~ALL"]
        self.assertEqual(audit.validate(rule), [])

    def test_main_explicitly_excluded_must_fail_even_with_all_scope(self):
        rule = safe_ruleset()
        rule["conditions"]["ref_name"]["include"] = ["~ALL"]
        rule["conditions"]["ref_name"]["exclude"] = ["refs/heads/main"]
        self.assertTrue(any("exclude" in s for s in audit.validate(rule)))

    def test_wildcard_exclusion_matching_main_is_rejected(self):
        rule = safe_ruleset()
        rule["conditions"]["ref_name"]["include"] = ["~ALL"]
        rule["conditions"]["ref_name"]["exclude"] = ["refs/heads/m*"]
        self.assertTrue(any("exclude" in s for s in audit.validate(rule)))

    def test_exclusion_for_other_branch_preserves_main(self):
        rule = safe_ruleset()
        rule["conditions"]["ref_name"]["include"] = ["~ALL"]
        rule["conditions"]["ref_name"]["exclude"] = ["refs/heads/automation/*"]
        self.assertEqual(audit.validate(rule), [])

    def test_missing_required_status_rule_is_rejected(self):
        rule = safe_ruleset()
        rule["rules"].pop()
        self.assertTrue(any("required status checks" in s for s in audit.validate(rule)))

    def test_missing_or_skipped_check_is_rejected(self):
        rule = safe_ruleset()
        checks = rule["rules"][-1]["parameters"]["required_status_checks"]
        checks.pop()
        checks.append({"context": "Benchmarks (regression-gated)", "integration_id": 15368})
        issues = audit.validate(rule)
        self.assertTrue(any("Analyze (Rust) (rust)" in s for s in issues))
        self.assertTrue(any("skipped" in s for s in issues))

    def test_untrusted_check_provider_is_rejected(self):
        rule = safe_ruleset()
        rule["rules"][-1]["parameters"]["required_status_checks"][0]["integration_id"] = -1
        self.assertTrue(any("provider" in s for s in audit.validate(rule)))

    def test_required_checks_need_strict_current_head(self):
        rule = safe_ruleset()
        rule["rules"][-1]["parameters"]["strict_required_status_checks_policy"] = False
        self.assertTrue(any("strict" in s for s in audit.validate(rule)))

    def test_missing_or_stale_review_controls_rejected(self):
        rule = safe_ruleset()
        rule["rules"][2]["parameters"].update({
            "required_approving_review_count": 0,
            "require_last_push_approval": False,
            "dismiss_stale_reviews_on_push": False,
            "required_review_thread_resolution": False,
        })
        issues = audit.validate(rule)
        self.assertGreaterEqual(len(issues), 4)

    def test_broad_role_bypass_rejected(self):
        rule = safe_ruleset()
        rule["bypass_actors"] = [{"actor_type": "RepositoryRole", "bypass_mode": "always"}]
        self.assertTrue(any("bypass" in s for s in audit.validate(rule)))

    def test_inactive_ruleset_rejected(self):
        rule = safe_ruleset()
        rule["enforcement"] = "disabled"
        self.assertTrue(any("active" in s for s in audit.validate(rule)))

    def test_malformed_payload_fails_closed(self):
        self.assertGreater(len(audit.validate({})), 0)
        self.assertGreater(len(audit.validate(None)), 0)


if __name__ == "__main__":
    unittest.main()

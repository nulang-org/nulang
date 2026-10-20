import contextlib
import http.server
import importlib.util
import io
import json
import pathlib
import sys
import unittest
import urllib.error
import threading

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "openhands_repair.py"

spec = importlib.util.spec_from_file_location("openhands_repair", SCRIPT)
openhands_repair = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = openhands_repair
spec.loader.exec_module(openhands_repair)


class CapturingOpener:
    def __init__(self, response):
        self.response = response
        self.request = None
        self.timeout = None

    def __call__(self, request, *, timeout):
        self.request = request
        self.timeout = timeout
        return io.StringIO(json.dumps(self.response))


class OpenHandsRepairTests(unittest.TestCase):
    def test_payload_starts_main_branch_agent_with_rust_verification_contract(self):
        payload = openhands_repair.conversation_payload(
            "nulang-org/nulang", "Repair the failing wasm test."
        )

        self.assertEqual("nulang-org/nulang", payload["selected_repository"])
        self.assertEqual("main", payload["selected_branch"])
        self.assertTrue(payload["initial_message"]["run"])
        prompt = payload["initial_message"]["content"][0]["text"]
        self.assertIn("Repair the failing wasm test.", prompt)
        self.assertIn("cargo fmt --all -- --check", prompt)
        self.assertIn("GitHub Actions remains the authoritative CI gate", prompt)
        self.assertIn("Never push to main", prompt)

    def test_start_conversation_posts_only_expected_payload_and_reports_ids(self):
        opener = CapturingOpener(
            {
                "id": "start-task-1",
                "status": "READY",
                "app_conversation_id": "conversation-1",
            }
        )

        result = openhands_repair.start_conversation(
            "api-key-not-to-log", "nulang-org/nulang", "Repair a focused test.", opener=opener
        )

        self.assertEqual("start-task-1", result.task_id)
        self.assertEqual("READY", result.status)
        self.assertEqual("conversation-1", result.conversation_id)
        self.assertEqual(30, opener.timeout)
        self.assertEqual("Bearer api-key-not-to-log", opener.request.get_header("Authorization"))
        payload = json.loads(opener.request.data)
        self.assertEqual("main", payload["selected_branch"])
        self.assertNotIn("secrets", payload)

    def test_api_error_does_not_echo_api_key_or_response_body(self):
        def failing_opener(request, *, timeout):
            raise urllib.error.HTTPError(
                openhands_repair.API_URL,
                401,
                "Unauthorized",
                hdrs=None,
                fp=io.BytesIO(b'{"detail":"untrusted response body"}'),
            )

        with self.assertRaises(openhands_repair.OpenHandsApiError) as raised:
            openhands_repair.start_conversation(
                "api-key-not-to-log", "nulang-org/nulang", "Repair a focused test.", opener=failing_opener
            )

        message = str(raised.exception)
        self.assertEqual("OpenHands API request failed with HTTP 401.", message)
        self.assertNotIn("api-key-not-to-log", message)
        self.assertNotIn("untrusted response body", message)

    def test_start_conversation_accepts_every_documented_non_error_status(self):
        expected_statuses = {
            "WORKING",
            "WAITING_FOR_SANDBOX",
            "PREPARING_REPOSITORY",
            "RUNNING_SETUP_SCRIPT",
            "SETTING_UP_GIT_HOOKS",
            "SETTING_UP_SKILLS",
            "STARTING_CONVERSATION",
            "READY",
        }
        self.assertEqual(expected_statuses, openhands_repair.START_TASK_STATUSES)

        for status in sorted(expected_statuses):
            with self.subTest(status=status):
                result = openhands_repair.start_conversation(
                    "api-key-not-to-log",
                    "nulang-org/nulang",
                    "Repair a focused test.",
                    opener=CapturingOpener({"id": "start-task-1", "status": status}),
                )
                self.assertEqual(status, result.status)

    def test_terminal_start_error_fails_closed(self):
        with self.assertRaises(openhands_repair.OpenHandsApiError) as raised:
            openhands_repair.start_conversation(
                "api-key-not-to-log",
                "nulang-org/nulang",
                "Repair a focused test.",
                opener=CapturingOpener({"id": "start-task-1", "status": "ERROR"}),
            )

        self.assertEqual("OpenHands API rejected the conversation start request.", str(raised.exception))

    def test_url_error_does_not_echo_api_key_or_response_reason(self):
        api_key = "api-key-not-to-log"

        def failing_opener(request, *, timeout):
            raise urllib.error.URLError(f"network failure for {api_key}")

        with self.assertRaises(openhands_repair.OpenHandsApiError) as raised:
            openhands_repair.start_conversation(
                api_key, "nulang-org/nulang", "Repair a focused test.", opener=failing_opener
            )

        message = str(raised.exception)
        self.assertEqual("OpenHands API request could not be completed.", message)
        self.assertNotIn(api_key, message)

    def test_default_opener_rejects_redirect_before_sending_bearer_to_new_origin(self):
        target_authorizations = []

        class TargetHandler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                target_authorizations.append(self.headers.get("Authorization"))
                self.send_response(200)
                self.end_headers()

            def log_message(self, format, *args):
                pass

        target_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), TargetHandler)
        target_thread = threading.Thread(target=target_server.serve_forever)
        target_thread.start()
        target_url = f"http://127.0.0.1:{target_server.server_port}/redirected"

        class RedirectHandler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.send_response(302)
                self.send_header("Location", target_url)
                self.end_headers()

            def log_message(self, format, *args):
                pass

        redirect_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), RedirectHandler)
        redirect_thread = threading.Thread(target=redirect_server.serve_forever)
        redirect_thread.start()
        original_api_url = openhands_repair.API_URL
        openhands_repair.API_URL = f"http://127.0.0.1:{redirect_server.server_port}/start"
        try:
            with self.assertRaises(openhands_repair.OpenHandsApiError) as raised:
                openhands_repair.start_conversation(
                    "api-key-not-to-log", "nulang-org/nulang", "Repair a focused test."
                )
        finally:
            openhands_repair.API_URL = original_api_url
            redirect_server.shutdown()
            redirect_server.server_close()
            redirect_thread.join()
            target_server.shutdown()
            target_server.server_close()
            target_thread.join()

        self.assertEqual("OpenHands API request failed with HTTP 302.", str(raised.exception))
        self.assertEqual([], target_authorizations)

    def test_malformed_api_key_fails_before_network_request_without_echoing_the_key(self):
        api_key = "api-key-not-to-log\r\ninjected-header"

        def unexpected_opener(request, *, timeout):
            self.fail("malformed API key must fail before opening a connection")

        with self.assertRaisesRegex(
            ValueError, "^OPENHANDS_API_KEY must use printable ASCII characters$"
        ):
            openhands_repair.start_conversation(
                api_key, "nulang-org/nulang", "Repair a focused test.", opener=unexpected_opener
            )

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = openhands_repair.main(
                {
                    "OPENHANDS_API_KEY": api_key,
                    "OPENHANDS_REPOSITORY": "nulang-org/nulang",
                    "OPENHANDS_TASK": "Repair a focused test.",
                }
            )

        self.assertEqual(1, result)
        self.assertEqual("OPENHANDS_API_KEY must use printable ASCII characters\n", stderr.getvalue())
        self.assertNotIn(api_key, stderr.getvalue())

    def test_payload_validation_is_not_relabelled_as_request_construction_failure(self):
        with self.assertRaisesRegex(ValueError, "^OPENHANDS_REPOSITORY is required$"):
            openhands_repair.start_conversation(
                "api-key-not-to-log", "", "Repair a focused test."
            )

    def test_missing_key_fails_before_network_request(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = openhands_repair.main(
                {
                    "OPENHANDS_REPOSITORY": "nulang-org/nulang",
                    "OPENHANDS_TASK": "Repair a focused test.",
                }
            )

        self.assertEqual(1, result)
        self.assertEqual("OPENHANDS_API_KEY is required\n", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()

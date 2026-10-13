"""Adversarial HTTP body parsing tests for the local Nulang playground.

These tests do not execute Nulang programs or start a listening socket.
They exercise the actual request handler with in-memory request streams.
"""

import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import Mock


ROOT = Path(__file__).resolve().parents[2]
SERVER = ROOT / "playground" / "server.py"
SPEC = importlib.util.spec_from_file_location("nulang_playground_server", SERVER)
playground = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(playground)


def request_handler(content_length: str, body: bytes = b""):
    handler = playground.Handler.__new__(playground.Handler)
    handler.headers = {"Content-Length": content_length}
    handler.rfile = io.BytesIO(body)
    handler.send_error_response = Mock()
    return handler


class PlaygroundRequestValidationTests(unittest.TestCase):
    def test_rejects_negative_content_length_without_reading_body(self):
        for raw_length in ("-1", "-1048576", " -2 "):
            with self.subTest(content_length=raw_length):
                handler = request_handler(raw_length, b'{"code":"safe"}')
                handler.rfile = Mock()
                self.assertIsNone(handler._read_json_body())
                handler.rfile.read.assert_not_called()
                handler.send_error_response.assert_called_once_with({
                    "ok": False, "stderr": "Invalid Content-Length header"
                })

    def test_rejects_non_numeric_and_oversized_lengths_before_read(self):
        for raw_length, error in (
            ("invalid", "Invalid Content-Length header"),
            (str(playground.MAX_CODE_BYTES + 4097), "Request body too large"),
        ):
            with self.subTest(content_length=raw_length):
                handler = request_handler(raw_length)
                handler.rfile = Mock()
                self.assertIsNone(handler._read_json_body())
                handler.rfile.read.assert_not_called()
                handler.send_error_response.assert_called_once_with({
                    "ok": False, "stderr": error
                })

    def test_accepts_valid_json_object_with_string_source(self):
        body = b'{"code": "perform IO.print(1)"}'
        handler = request_handler(str(len(body)), body)
        self.assertEqual(
            handler._read_json_body(), {"code": "perform IO.print(1)"}
        )
        handler.send_error_response.assert_not_called()

    def test_rejects_non_object_json(self):
        body = b'["not-an-object"]'
        handler = request_handler(str(len(body)), body)
        self.assertIsNone(handler._read_json_body())
        handler.send_error_response.assert_called_once()

    def test_rejects_non_string_source_before_running_compiler(self):
        for value in ("null", "42", "[]", "{}"):
            with self.subTest(code=value):
                body = ('{"code":' + value + '}').encode()
                handler = request_handler(str(len(body)), body)
                self.assertIsNone(handler._read_json_body())
                handler.send_error_response.assert_called_once_with({
                    "ok": False, "stderr": "Source code must be a string"
                })

    def test_rejects_non_string_compile_target_without_subprocess(self):
        handler = playground.Handler.__new__(playground.Handler)
        handler._read_json_body = Mock(return_value={"code": "1", "target": []})
        handler.send_error_response = Mock()
        handler._run_nulang = Mock()
        handler.handle_compile()
        handler._run_nulang.assert_not_called()
        handler.send_error_response.assert_called_once()
        self.assertIn("Invalid target", handler.send_error_response.call_args.args[0]["stderr"])


if __name__ == "__main__":
    unittest.main()

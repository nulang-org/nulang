"""Local, deterministic correctness tests for the diagnostic HTTP harness."""

import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import web_http_bench as bench


class SampleHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/broken":
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path == "/no-etag":
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
            return
        revalidated = self.headers.get("If-None-Match") == '"fixture-v1"'
        self.send_response(304 if revalidated else 200)
        self.send_header("ETag", '"fixture-v1"')
        self.send_header("Content-Length", "0" if revalidated else "2")
        self.end_headers()
        if not revalidated:
            self.wfile.write(b"ok")

    def log_message(self, format, *args):
        pass


class WebHttpBenchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), SampleHandler)
        cls.thread = threading.Thread(target=cls.httpd.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.httpd.server_close()
        cls.thread.join()

    def test_baseline_and_revalidation_report_distinct_body_sizes(self):
        normal, elapsed = bench.run_phase("127.0.0.1", self.httpd.server_port,
                                           "/asset.js", 8, 2, 5, None)
        summary = bench.summarize(normal, elapsed, 200)
        self.assertEqual(summary["requests"], 8)
        self.assertEqual(summary["failures"], 0)
        self.assertEqual(summary["statuses"], {"200": 8})
        self.assertEqual(summary["body_bytes_total"], 16)
        etag_obs, tag = bench.request_once("127.0.0.1", self.httpd.server_port,
                                            "/asset.js", 5)
        self.assertEqual(etag_obs.status, 200)
        self.assertEqual(tag, '"fixture-v1"')
        conditional, elapsed = bench.run_phase("127.0.0.1", self.httpd.server_port,
                                               "/asset.js", 8, 2, 5, tag)
        summary = bench.summarize(conditional, elapsed, 304)
        self.assertEqual(summary["failures"], 0)
        self.assertEqual(summary["body_bytes_total"], 0)
        self.assertEqual(summary["statuses"], {"304": 8})

    def test_failed_responses_remain_in_denominator(self):
        observations, elapsed = bench.run_phase("127.0.0.1", self.httpd.server_port,
                                                "/broken", 5, 2, 5, None)
        report = bench.summarize(observations, elapsed, 200)
        self.assertEqual(report["requests"], 5)
        self.assertEqual(report["failures"], 5)
        self.assertEqual(report["statuses"], {"503": 5})
        self.assertEqual(report["latency_ms_p50"], 0)

    def test_cli_generates_json_report(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            with contextlib.redirect_stdout(io.StringIO()):
                code = bench.main(["--port", str(self.httpd.server_port), "--path", "/asset.js",
                                   "--requests", "4", "--warmup", "1", "--concurrency", "2",
                                   "--revalidate", "--label", "test-commit", "--output", str(output)])
            self.assertEqual(code, 0)
            data = json.loads(output.read_text())
            self.assertEqual(data["label"], "test-commit")
            self.assertEqual(data["statuses"], {"304": 4})
            self.assertEqual(data["body_bytes_total"], 0)

    def test_preflight_missing_etag_fails_closed(self):
        with contextlib.redirect_stderr(io.StringIO()):
            code = bench.main(["--port", str(self.httpd.server_port), "--path", "/no-etag",
                               "--requests", "2", "--warmup", "0", "--revalidate"])
        self.assertEqual(code, 2)

    def test_nearest_rank_percentile_and_empty_samples(self):
        self.assertEqual(bench.percentile_ns([100, 200, 300, 400], .5), 200)
        self.assertEqual(bench.percentile_ns([100, 200, 300, 400], .95), 400)
        self.assertEqual(bench.percentile_ns([], .95), 0)

    def test_invalid_cli_limits(self):
        for arguments in (["--port", "0"], ["--port", "1234", "--concurrency", "999"],
                          ["--port", "1234", "--path", "//elsewhere"]):
            with contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    bench.main(arguments)


if __name__ == "__main__":
    unittest.main()

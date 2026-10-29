#!/usr/bin/env python3
"""Real-browser regression runner for the dependency-free nulang-ui DOM host."""
from __future__ import annotations

import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from threading import Thread

ROOT = Path(__file__).resolve().parent


def main() -> int:
    browser = next((path for name in ("chromium-browser", "chromium", "google-chrome")
                    if (path := shutil.which(name))), None)
    if not browser:
        print("Chromium not found; browser regression tests cannot run")
        return 2

    class Handler(http.server.SimpleHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/__test__/live":
                # Real browser fetch -> real loopback HTTP -> framed SSE,
                # with a test-only credential gate. Not a production endpoint.
                if self.headers.get("Authorization") != "Bearer test-token":
                    self.send_response(403)
                    self.send_header("Cache-Control", "no-store")
                    self.end_headers()
                    return
                nodes = [
                    {"id":"root","kind":"div","properties":{},"children":["a","b"]},
                    {"id":"a","kind":"span","properties":{"text":{"type":"string","value":"A"}},"children":[]},
                    {"id":"b","kind":"span","properties":{"text":{"type":"string","value":"B"}},"children":[]}
                ]
                messages = [
                    {"protocol":"nulang-ui-msg/1","type":"snapshot","document":{
                        "protocol":"nulang-ui/1","document_id":"test","revision":"1","root":"root","nodes":nodes
                    }},
                    {"protocol":"nulang-ui-msg/1","type":"patch","patch":{
                        "protocol":"nulang-ui/1","document_id":"test","base_revision":"1","revision":"2",
                        "operations":[{"op":"replace_children","node_id":"root","children":["b","a"]}]
                    }}
                ]
                frames = "".join(
                    "event: nulang-ui\nid: {}\ndata: {}\n\n".format(
                        i, json.dumps(msg, separators=(",",":"))
                    ) for i,msg in enumerate(messages,start=1)
                ).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type","text/event-stream; charset=utf-8")
                self.send_header("Cache-Control","no-store")
                self.send_header("Content-Length",str(len(frames)))
                self.end_headers()
                midpoint=len(frames)//2
                for chunk in (frames[:midpoint],frames[midpoint:]):
                    self.wfile.write(chunk)
                    self.wfile.flush()
                return
            return super().do_GET()

        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(ROOT), **kwargs)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    temp_parent = os.environ.get("NULANG_BROWSER_TEST_TMP")
    if temp_parent:
        Path(temp_parent).mkdir(parents=True, exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix="nd-", dir=temp_parent) as temp:
            env = {**os.environ, "TMPDIR": temp}
            result = subprocess.run(
                [browser, "--headless", "--no-sandbox", "--disable-gpu",
                 "--disable-dev-shm-usage", "--no-first-run",
                 "--disable-background-networking", f"--user-data-dir={temp}",
                 "--virtual-time-budget=3000", "--dump-dom",
                 f"http://127.0.0.1:{server.server_port}/dom-host.test.html"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                timeout=35, env=env, check=False,
            )
            output = result.stdout
            summary = output.partition('<pre id="results">')[2].partition("</pre>")[0]
            print(summary or result.stderr[-2000:])
            if result.returncode or 'data-test-status="passed"' not in output or "ALL PASS" not in summary:
                return 1
            return 0
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    raise SystemExit(main())

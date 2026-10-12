#!/usr/bin/env python3
"""Run Python and Node HTTP clients against a REAL local Nulang HTTP server.

Checks transport compatibility, not compiler-generated Web route conformance.
No public network, provider credentials, or privileged ports are required.
"""
import argparse
import os
from pathlib import Path
import re
import select
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from interop.web.python.client import NulangWebClient  # noqa: E402

CONTRACT = {
    "openapi": "3.1.0",
    "paths": {
        "/probe/{id}": {"get": {
            "operationId": "read_probe",
            "parameters": [
                {"name": "id", "in": "path", "required": True},
                {"name": "tag", "in": "query"},
                {"name": "x-tenant-id", "in": "header", "required": True},
            ],
        }},
    },
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nulang-bin", default="nulang")
    parser.add_argument("--timeout", type=float, default=15)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    binary = shutil.which(args.nulang_bin)
    if not binary:
        parser.error(f"Nulang executable not found: {args.nulang_bin}")
    binary = str(Path(binary).resolve())
    script = ROOT / "examples" / "web_live_interop.nula"
    command = [binary, "--backend", "bytecode", "--with", "net", str(script)]
    process = subprocess.Popen(command, cwd=ROOT, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, bufsize=1)
    try:
        assert process.stdout is not None
        deadline = time.monotonic() + args.timeout
        port = None
        seen = []
        while port is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError(f"Nulang server port marker timed out: {seen}")
            ready, _, _ = select.select([process.stdout], [], [], remaining)
            if not ready:
                raise RuntimeError(f"Nulang server did not report a port: {seen}")
            line = process.stdout.readline()
            if not line:
                _, stderr = process.communicate(timeout=5)
                raise RuntimeError(f"Nulang server exited rc={process.returncode}; "
                                   f"output={seen}; stderr={stderr.strip()}")
            seen.append(line.strip())
            match = re.fullmatch(r"PORT=(\d+)", line.strip())
            if match:
                port = int(match.group(1))
                if not 1 <= port <= 65535:
                    raise RuntimeError(f"Nulang returned an invalid HTTP port: {port}")
        if process.poll() is not None:
            raise RuntimeError("Nulang server exited after announcing its port")
        base = f"http://127.0.0.1:{port}"
        client = NulangWebClient(base, CONTRACT, token="local-smoke-token", timeout=4)
        response = client.call("read_probe", path={"id": "a/b"},
                               query={"tag": ["one two", "é"]},
                               headers={"x-tenant-id": "local-smoke"},
                               traceparent="00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        if response.status != 200 or response.text() != "nulang-web-live":
            raise RuntimeError(f"Python HTTP client returned unexpected status/body: {response.status}, {response.text()!r}")
        print("PASS Python client -> native Nulang HTTP server", flush=True)
        node = subprocess.run(["node", str(ROOT / "interop/web/javascript/live_server_smoke.mjs"), base],
                              cwd=ROOT, capture_output=True, text=True, timeout=args.timeout)
        if node.returncode != 0:
            raise RuntimeError(f"Node client failed rc={node.returncode}: "
                               f"stdout={node.stdout}; stderr={node.stderr}")
        print(node.stdout.strip(), flush=True)
        return 0
    finally:
        if process.poll() is None:
            process.kill()
        process.communicate(timeout=5)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.TimeoutExpired) as exc:
        print(f"FAIL native Nulang Web interop: {exc}", file=sys.stderr)
        raise SystemExit(1)

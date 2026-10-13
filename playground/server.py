#!/usr/bin/env python3
"""Nulang Playground Server — serves index.html and runs Nulang code via POST /run, /compile, /check.

Security notes:
  - Subprocesses are spawned with an argv list (no shell=True), a fresh temp
    working directory per request, a wall-clock timeout, POSIX rlimits
    (CPU time, address space, file size, no core dumps), and capped output.
  - NOTE FOR PRODUCTION DEPLOYMENTS: rlimits alone are NOT sufficient
    isolation for running untrusted code. Deploy this behind an additional
    sandbox layer such as a container (gVisor/Kata), seccomp-bpf syscall
    filter, network namespace with no egress, and a dedicated unprivileged
    user. The limits below are defense-in-depth, not a security boundary.

Usage:
    python3 playground/server.py [--port 8080] [--nulang ./target/debug/nulang]
"""

import http.server
import json
import os
import resource
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PLAYGROUND_DIR = Path(__file__).resolve().parent
INDEX_HTML = (PLAYGROUND_DIR / "index.html").read_text()
NULANG_BIN = os.environ.get("NULANG_PATH", None)

# Try to find the nulang binary
if not NULANG_BIN:
    candidates = [
        Path(__file__).resolve().parent.parent / "target" / "debug" / "nulang",
        Path(__file__).resolve().parent.parent / "target" / "release" / "nulang",
    ]
    for c in candidates:
        if c.exists():
            NULANG_BIN = str(c)
            break
    if not NULANG_BIN:
        # Try to find in PATH
        NULANG_BIN = shutil.which("nulang") or "nulang"

# --- Sandbox limits (defense-in-depth; see security notes in the docstring) ---
TIMEOUT_SECONDS = 30            # wall-clock limit per subprocess
CPU_LIMIT_SECONDS = 20          # RLIMIT_CPU: hard CPU-time cap
ADDRESS_SPACE_BYTES = 1 << 30   # RLIMIT_AS: 1 GiB virtual memory cap
FILE_SIZE_BYTES = 16 << 20      # RLIMIT_FSIZE: 16 MiB max file writes
MAX_CODE_BYTES = 256 * 1024     # reject request bodies with more code than this
MAX_OUTPUT_CHARS = 100_000      # cap stdout/stderr returned to the client

# Whitelist of compile targets accepted by /compile (matches --target in main.rs).
_ALLOWED_COMPILE_TARGETS = {"native", "riscv64", "ptx"}


def _apply_rlimits():
    """preexec_fn for subprocess: restrict the child process (POSIX only)."""
    resource.setrlimit(resource.RLIMIT_CPU, (CPU_LIMIT_SECONDS, CPU_LIMIT_SECONDS))
    resource.setrlimit(resource.RLIMIT_AS, (ADDRESS_SPACE_BYTES, ADDRESS_SPACE_BYTES))
    resource.setrlimit(resource.RLIMIT_FSIZE, (FILE_SIZE_BYTES, FILE_SIZE_BYTES))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))  # no core dumps


def _truncate(s, limit=MAX_OUTPUT_CHARS):
    if len(s) > limit:
        return s[:limit] + f"\n... [output truncated at {limit} chars]"
    return s


class Handler(http.server.BaseHTTPRequestHandler):
    def send_json(self, data):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())

    def send_error_response(self, data):
        self.send_response(400)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())

    def do_GET(self):
        if self.path == "/" or self.path == "/index.html":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(INDEX_HTML.encode())
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        if self.path == "/run":
            self.handle_run()
        elif self.path == "/compile":
            self.handle_compile()
        elif self.path == "/check":
            self.handle_check()
        else:
            self.send_response(404)
            self.end_headers()

    def _read_json_body(self):
        """Parse the JSON request body with a size cap. Returns None on error."""
        try:
            content_length = int(self.headers.get('Content-Length', 0))
        except (ValueError, TypeError):
            self.send_error_response({'ok': False, 'stderr': 'Invalid Content-Length header'})
            return None

        # Buffered readers interpret read(-1) as read-until-EOF, which can
        # indefinitely block or consume unbounded input on an HTTP connection.
        if content_length < 0:
            self.send_error_response({'ok': False, 'stderr': 'Invalid Content-Length header'})
            return None

        if content_length > MAX_CODE_BYTES + 4096:
            self.send_error_response({'ok': False, 'stderr': 'Request body too large'})
            return None
        body = self.rfile.read(content_length).decode('utf-8', errors='replace')
        try:
            data = json.loads(body)
        except (ValueError, TypeError):
            self.send_error_response({'ok': False, 'stderr': 'Invalid JSON'})
            return None

        if not isinstance(data, dict):
            self.send_error_response({'ok': False, 'stderr': 'Request body must be a JSON object'})
            return None

        code = data.get('code', '')
        if not isinstance(code, str):
            self.send_error_response({'ok': False, 'stderr': 'Source code must be a string'})
            return None
        if len(code.encode('utf-8', errors='replace')) > MAX_CODE_BYTES:
            self.send_error_response({'ok': False, 'stderr': 'Source code too large'})
            return None
        return data

    def _run_nulang(self, argv, code, timeout_msg):
        """Write `code` to a fresh temp dir and run `nulang` with the given argv.

        Uses an argv list (no shell), a per-request temp working directory,
        rlimits, a timeout, and output caps. The temp dir (and the source
        file inside it) is always cleaned up.
        """
        workdir = tempfile.mkdtemp(prefix="nulang-playground-")
        try:
            # Fixed, safe filename inside an isolated directory — the source
            # content comes from the request, the path does not.
            src_file = os.path.join(workdir, "main.nula")
            with open(src_file, "w") as f:
                f.write(code)

            result = subprocess.run(
                [NULANG_BIN, *argv, src_file],
                capture_output=True,
                text=True,
                timeout=TIMEOUT_SECONDS,
                cwd=workdir,
                preexec_fn=_apply_rlimits,
            )

            self.send_json({
                'ok': result.returncode == 0,
                'stdout': _truncate(result.stdout),
                'stderr': _truncate(result.stderr)
            })
        except subprocess.TimeoutExpired:
            self.send_json({'ok': False, 'stderr': timeout_msg})
        except Exception as e:
            self.send_json({'ok': False, 'stderr': str(e)})
        finally:
            shutil.rmtree(workdir, ignore_errors=True)

    def handle_run(self):
        data = self._read_json_body()
        if data is None:
            return
        code = data.get('code', '')

        # Use the default bytecode VM backend (`nulang file.nula`), which
        # supports the full language including effects (IO) and actors. The
        # `--backend native` AOT backend only supports a pure-functional
        # subset and rejects effectful code, so it cannot run most real
        # programs (even hello-world uses `perform IO.print`). Use /compile
        # for explicit backend/target selection instead.
        self._run_nulang([], code, f'Execution timed out ({TIMEOUT_SECONDS}s)')

    def handle_compile(self):
        data = self._read_json_body()
        if data is None:
            return
        code = data.get('code', '')
        target = data.get('target', 'native')

        # Whitelist the target so client input can never inject extra flags
        # or unexpected values into the compiler invocation.
        if not isinstance(target, str) or target not in _ALLOWED_COMPILE_TARGETS:
            self.send_error_response({
                'ok': False,
                'stderr': f'Invalid target {target!r}; allowed: {sorted(_ALLOWED_COMPILE_TARGETS)}'
            })
            return

        # Note: the native AOT backend only supports a pure-functional
        # subset; effectful programs will fail with an unsupported-construct
        # error. That is expected — /compile is for explicit backend use.
        self._run_nulang(
            ['--backend', 'native', '--target', target, '--emit-nbc'],
            code,
            f'Compilation timed out ({TIMEOUT_SECONDS}s)'
        )

    def handle_check(self):
        data = self._read_json_body()
        if data is None:
            return
        code = data.get('code', '')
        self._run_nulang(['--check'], code, f'Type check timed out ({TIMEOUT_SECONDS}s)')

    def log_message(self, format, *args):
        sys.stderr.write("[playground] %s\n" % (format % args))


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument('--port', type=int, default=8080)
    parser.add_argument('--nulang', type=str, default=None)
    args = parser.parse_args()

    if args.nulang:
        NULANG_BIN = args.nulang

    port = args.port
    server = http.server.HTTPServer(('', port), Handler)
    print(f"Nulang Playground running on http://localhost:{port}")
    print(f"Using nulang binary: {NULANG_BIN}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down...")
        server.server_close()

#!/usr/bin/env python3
"""Real-process smoke for the generated Nulang HTTP service.

Connects through loopback to a temporary service on an ephemeral port.
The current dev server may bind all interfaces: run only in isolated CI.
Requires an already-built Nulang executable and no remote services.
"""
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def main() -> int:
    binary = Path(os.environ.get("NULANG_BIN", "target/release/nulang")).resolve()
    if not binary.is_file():
        print(f"missing Nulang executable: {binary}", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="nulang-service-smoke-") as temp:
        package = Path(temp) / "smoke-service"
        subprocess.run(
            [str(binary), "nula", "new", str(package), "--template", "service"],
            check=True,
            timeout=30,
        )
        assert (package / "src" / "main.nula").is_file()
        assert (package / "README.md").is_file()

        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            port = reserve.getsockname()[1]

        log_path = Path(temp) / "service.log"
        with log_path.open("w+", encoding="utf-8") as log:
            server = subprocess.Popen(
                [str(binary), "nula", "dev", "--port", str(port)],
                cwd=package,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 30
                last_error = None
                while time.monotonic() < deadline:
                    if server.poll() is not None:
                        raise RuntimeError(f"server exited with {server.returncode}")
                    try:
                        with urllib.request.urlopen(
                            f"http://127.0.0.1:{port}/health", timeout=1
                        ) as response:
                            body = response.read().decode("utf-8")
                            assert response.status == 200
                            assert "ok" in body, f"unexpected /health response: {body!r}"
                        break
                    except (OSError, urllib.error.URLError) as exc:
                        last_error = exc
                        time.sleep(0.2)
                else:
                    raise RuntimeError(f"service did not start: {last_error}")

                with urllib.request.urlopen(
                    f"http://127.0.0.1:{port}/", timeout=3
                ) as response:
                    homepage = response.read().decode("utf-8")
                    assert response.status == 200
                    assert "Nulang Service" in homepage, homepage
                # The service template explicitly opts into SQLite state.
                write_request = urllib.request.Request(
                    f"http://127.0.0.1:{port}/state",
                    data=b"value=committed-after-restart",
                    headers={"Content-Type": "application/x-www-form-urlencoded"},
                    method="POST",
                )
                with urllib.request.urlopen(write_request, timeout=3) as response:
                    assert response.status == 200
                    assert "saved" in response.read().decode("utf-8")

                with urllib.request.urlopen(
                    f"http://127.0.0.1:{port}/state", timeout=3
                ) as response:
                    assert response.status == 200
                    assert "committed-after-restart" in response.read().decode("utf-8")

                db_file = package / "data" / "web-state.sqlite"
                assert db_file.is_file(), "opt-in SQLite backend must create a real database file"

                # Hard-kill the process AFTER a successful HTTP acknowledgement.
                # Restart from the same database to prove that the web request
                # path does not silently use the in-memory demo KV backend.
                server.kill()
                server.wait(timeout=5)
                server = subprocess.Popen(
                    [str(binary), "nula", "dev", "--port", str(port)],
                    cwd=package,
                    stdin=subprocess.DEVNULL,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                )

                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    if server.poll() is not None:
                        raise RuntimeError(f"restarted server exited with {server.returncode}")
                    try:
                        with urllib.request.urlopen(
                            f"http://127.0.0.1:{port}/state", timeout=1
                        ) as response:
                            body = response.read().decode("utf-8")
                            assert response.status == 200
                            assert "committed-after-restart" in body, (
                                f"acknowledged web write lost after restart: {body!r}"
                            )
                        break
                    except (OSError, urllib.error.URLError):
                        time.sleep(0.2)
                else:
                    raise RuntimeError("restart did not serve persisted web state")

                print("service template smoke: HTTP routes + acknowledged SQLite write survives SIGKILL")
                return 0
            except Exception:
                log.flush()
                print("service output:\n" + log_path.read_text(encoding="utf-8"), file=sys.stderr)
                raise
            finally:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Exercise actual Nulang Web route contracts over Python and JavaScript HTTP.

Unlike smoke_web_client_interop.py, the OpenAPI document is extracted from
the exact package whose typed route is served by 'nula dev'. No mocked schema.
"""
import argparse
import json
from pathlib import Path
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "examples" / "web_contract_interop"
sys.path.insert(0, str(ROOT))
from interop.web.python.client import ApiError, NulangWebClient  # noqa: E402

READY = re.compile(r"Dev server listening on http://127[.]0[.]0[.]1:(\d+)/")
EXPECTED_PATH = "/probe/{id}"


def export_contract(timeout: float) -> dict:
    command = [
        "cargo", "run", "--locked", "--no-default-features",
        "--example", "export_web_openapi", "--", str(FIXTURE / "src"),
    ]
    result = subprocess.run(
        command, cwd=ROOT, capture_output=True, text=True, timeout=timeout,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(
            f"contract export failed rc={result.returncode}: {result.stderr.strip()}"
        )
    contract = json.loads(result.stdout)
    operation = contract.get("paths", {}).get(EXPECTED_PATH, {}).get("get", {})
    if operation.get("operationId") != "read_probe":
        raise RuntimeError("exported OpenAPI omitted the expected typed read_probe route")
    return contract


def server_port(process: subprocess.Popen, timeout: float) -> int:
    assert process.stdout is not None
    deadline = time.monotonic() + timeout
    output = []
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError(f"nula dev startup timed out: {output}")
        ready, _, _ = select.select([process.stdout], [], [], remaining)
        if not ready:
            raise RuntimeError(f"nula dev did not report a port: {output}")
        line = process.stdout.readline()
        if not line:
            stderr = process.stderr.read() if process.stderr else ""
            raise RuntimeError(
                f"nula dev exited rc={process.poll()}; output={output}; "
                f"stderr={stderr[-4000:]}"
            )
        output.append(line.strip())
        found = READY.fullmatch(line.strip())
        if found:
            port = int(found.group(1))
            if not 1 <= port <= 65535:
                raise RuntimeError(f"nula dev advertised invalid port {port}")
            if process.poll() is not None:
                raise RuntimeError("nula dev exited after advertising its port")
            return port


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nulang-bin", required=True)
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    binary = shutil.which(args.nulang_bin)
    if not binary:
        parser.error(f"Nulang executable not found: {args.nulang_bin}")

    contract = export_contract(args.timeout)
    with tempfile.TemporaryDirectory(prefix="nulang-contract-interop-") as folder:
        contract_file = Path(folder) / "openapi.json"
        contract_file.write_text(json.dumps(contract), encoding="utf-8")
        process = subprocess.Popen(
            [str(Path(binary).resolve()), "nula", "dev", "--port", "0"],
            cwd=FIXTURE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        try:
            port = server_port(process, args.timeout)
            base = f"http://127.0.0.1:{port}"
            client = NulangWebClient(base, contract, timeout=5)
            response = client.call("read_probe", path={"id": 42})
            if response.status != 200 or response.text().strip() != "42":
                raise RuntimeError(
                    f"typed route returned unexpected result: {response.status} {response.text()!r}"
                )
            try:
                client.call("read_probe", path={"id": "invalid"})
            except ApiError as error:
                if error.status != 400 or json.loads(error.body).get("code") != "invalid_request_input":
                    raise RuntimeError(
                        f"typed invalid-path response was not a 400 problem: {error.status}"
                    ) from error
            else:
                raise RuntimeError("invalid path was accepted by typed Nulang route")
            print("PASS exported contract -> typed Nulang route -> Python client", flush=True)
            node = subprocess.run(
                ["node", str(ROOT / "interop/web/javascript/contract_server_smoke.mjs"),
                 base, str(contract_file)],
                cwd=ROOT, capture_output=True, text=True, timeout=args.timeout,
            )
            if node.returncode:
                raise RuntimeError(
                    f"JS typed Web contract smoke failed rc={node.returncode}: "
                    f"{node.stdout} {node.stderr}"
                )
            print(node.stdout.strip(), flush=True)
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=5)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.TimeoutExpired, ValueError) as exc:
        print(f"FAIL typed Nulang Web interop: {exc}", file=sys.stderr)
        raise SystemExit(1)

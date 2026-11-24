"""Run each Nulang conformance fixture with an independent durable store."""

import os
import subprocess
import tempfile


def run_isolated(nulang_bin, nula_path):
    """Preserve the fixture's cwd and output, but never reuse durable history."""
    with tempfile.TemporaryDirectory(prefix="nulang-conformance-store-") as store:
        env = os.environ.copy()
        env["NULANG_STORE_PATH"] = store
        return subprocess.run(
            [nulang_bin, nula_path],
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )

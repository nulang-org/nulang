#!/usr/bin/env python3
"""Compare Nulang Core bootstrap generations and execute a curated corpus.

Requires an already-built \`nulang\` binary. No network and no repository writes.
This is *bootstrap-generation* parity, not Rust-frontend or full-language parity.
"""

import argparse
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / 'bootstrap' / 'fixtures' / 'differential.json'
TIMEOUT_SECONDS = 240


def instructions(output: str) -> str:
    """Mirror verify.sh's check-7 oracle: ignore comments and line endings."""
    result = ''.join(line for line in output.splitlines() if not line.startswith(';'))
    if not result.strip():
        raise ValueError('empty compiler emission')
    return result


def last_result(output: str) -> str:
    lines = [line.strip() for line in output.splitlines() if line.strip()]
    if not lines:
        raise ValueError('empty VM output')
    return lines[-1]


def load_cases(path: Path) -> list[dict[str, str]]:
    cases = json.loads(path.read_text(encoding='utf-8'))
    if not isinstance(cases, list) or not cases:
        raise ValueError('corpus must be a non-empty JSON array')
    seen = set()
    for case in cases:
        if not isinstance(case, dict) or any(
            not isinstance(case.get(field), str) or not case[field].strip()
            for field in ('id', 'source', 'expected')
        ):
            raise ValueError('each corpus case requires non-empty id, source, expected strings')
        if re.fullmatch(r'[a-z0-9_]+', case['id']) is None:
            raise ValueError(f'unsafe corpus id: {case["id"]!r}')
        if case['id'] in seen:
            raise ValueError(f'duplicate corpus id: {case["id"]}')
        seen.add(case['id'])
    return cases


def run(command: list[str], data: bytes = b'') -> bytes:
    try:
        result = subprocess.run(command, cwd=ROOT, input=data, capture_output=True,
                                timeout=TIMEOUT_SECONDS, env=os.environ.copy(), check=False)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f'timeout running {command[0]} ({TIMEOUT_SECONDS}s)') from error
    if result.returncode:
        stderr = result.stderr.decode('utf-8', errors='replace')[-3000:]
        raise RuntimeError(f'{command!r} exited {result.returncode}: {stderr}')
    return result.stdout


def assemble(hex_stream: bytes) -> bytes:
    patched = run([sys.executable, 'bootstrap/fixup_hex.py'], hex_stream)
    binary = run([sys.executable, 'bootstrap/hex2nbc.py'], patched)
    if not binary:
        raise ValueError('empty .nbc artifact')
    return binary


def verify(nulang: Path, corpus: Path) -> None:
    cases = load_cases(corpus)
    # Stage 2 requires the Nulang emitter to compile its preprocessed source.
    # Only the already-built Rust host binary executes the bootstrap programs.
    prepared = run([sys.executable, 'bootstrap/prep_core.py'],
                   (ROOT / 'bootstrap/compile_hex.nula').read_bytes())
    stage2 = assemble(run([str(nulang), 'bootstrap/compile_hex.nula'], prepared))
    with tempfile.TemporaryDirectory(prefix='nulang-bootstrap-') as work:
        stage2_path = Path(work) / 'self_compile.nbc'
        stage2_path.write_bytes(stage2)
        for case in cases:
            source = case['source'].encode('utf-8')
            host_hex = run([str(nulang), 'bootstrap/compile_hex.nula'], source)
            self_hex = run([str(nulang), str(stage2_path)], source)
            if instructions(host_hex.decode('utf-8')) != instructions(self_hex.decode('utf-8')):
                raise AssertionError(f'{case["id"]}: Stage 1 and Stage 2 bytecode emissions differ')
            for generation, output in (('stage1', host_hex), ('stage2', self_hex)):
                artifact = Path(work) / f'{case["id"]}-{generation}.nbc'
                artifact.write_bytes(assemble(output))
                actual = last_result(run([str(nulang), str(artifact)]).decode('utf-8'))
                if actual != case['expected']:
                    raise AssertionError(
                        f'{case["id"]} ({generation}): expected {case["expected"]!r}, got {actual!r}'
                    )
            print(f'PASS {case["id"]}: both generations returned {case["expected"]}', flush=True)
    print(f'PASS bootstrap differential corpus: {len(cases)} cases', flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--nulang', required=True, type=Path, help='path to built nulang binary')
    parser.add_argument('--corpus', type=Path, default=CORPUS)
    arguments = parser.parse_args()
    nulang = arguments.nulang.resolve()
    if not nulang.is_file():
        parser.error(f'nulang binary does not exist: {nulang}')
    os.environ.setdefault('NULANG_STEP_LIMIT', '400000000')
    try:
        verify(nulang, arguments.corpus)
    except (AssertionError, RuntimeError, ValueError, UnicodeDecodeError) as error:
        print(f'FAIL bootstrap differential: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())

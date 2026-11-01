#!/usr/bin/env python3
"""Compare Nulang Core bootstrap generations and execute a curated corpus.

Requires an already-built `nulang` binary. No network and no repository writes.
This is *bootstrap-generation* parity, not Rust-frontend or full-language parity.
"""

import argparse
import hashlib
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



def first_difference(left: bytes | str, right: bytes | str) -> int:
    """First differing index, including at EOF when one value is a prefix."""
    for offset, (a, b) in enumerate(zip(left, right)):
        if a != b:
            return offset
    return min(len(left), len(right)) if len(left) != len(right) else -1


def emission_context(hex_text: str, target_word: int, radius: int = 6) -> str:
    """Bounded, word-indexed context for a compiler-generation divergence."""
    count = (len(hex_text) + 7) // 8
    begin = max(0, target_word - radius)
    end = min(count, target_word + radius + 1)
    entries = []
    for index in range(begin, end):
        word = hex_text[index * 8:(index + 1) * 8]
        value = f'[{word}]' if index == target_word else word
        entries.append(f'{index}:{value}')
    if target_word >= count:
        entries.append(f'{target_word}:[<eof>]')
    return ' '.join(entries)


def marker_context(raw_emission: str, target_word: int, limit: int = 3) -> str:
    """Nearest comments preceding an instruction, with their word offsets."""
    word_index = 0
    recent = []
    for line in raw_emission.splitlines():
        if line.startswith(';'):
            if word_index > target_word:
                break
            recent.append(f'@{word_index} {line[:100]}')
            recent = recent[-limit:]
        elif line.strip():
            if word_index >= target_word:
                break
            word_index += 1
    return ' | '.join(recent) or '<none>'


def assert_emissions_equal(case_id: str, host_hex: bytes, self_hex: bytes) -> None:
    """Fail closed, identifying the first divergent word in a corpus case."""
    host = instructions(host_hex.decode('utf-8'))
    self_hosted = instructions(self_hex.decode('utf-8'))
    offset = first_difference(host, self_hosted)
    if offset == -1:
        return
    word_index = offset // 8
    word_start = word_index * 8
    raise AssertionError(
        f'{case_id}: Stage 1 and Stage 2 bytecode emissions differ at word {word_index}'
        f' (hex character {offset}); '
        f'Stage 1 word={host[word_start:word_start + 8]}; '
        f'Stage 2 word={self_hosted[word_start:word_start + 8]}; '
        f'Stage 1 nearby: {emission_context(host, word_index)}; '
        f'Stage 2 nearby: {emission_context(self_hosted, word_index)}; '
        f'Stage 1 recent markers: {marker_context(host_hex.decode("utf-8"), word_index)}; '
        f'Stage 2 recent markers: {marker_context(self_hex.decode("utf-8"), word_index)}'
    )


def compiler_prefix_source(prepared: bytes) -> bytes:
    """Keep all preceding compiler helpers and the exact curried instr body.

    Removing a source declaration perturbs register layout, so this probe
    preserves the original preprocessor output until the next top-level let.
    """
    text = prepared.decode('utf-8')
    boundary = ' in let emit_word ='
    if (not text.startswith('let main = fn() => (let ')
            or text.count(boundary) != 1
            or text.count(' in let instr =') != 1):
        raise ValueError('compiler prefix markers missing or ambiguous')
    return (text.split(boundary, 1)[0] +
            ' in instr(82)(25)(7)(0)) in main()').encode('utf-8')


def compiler_context_source(prepared: bytes, boundary: str) -> bytes:
    """Preserve full top-level compiler helper declarations before `boundary`.

    Generated helper bindings are at parenthesis depth one under main's body.
    Parsing this boundary structurally avoids splitting nested local lets.
    This is an on-failure *probe*, not a transformation used to build Stage 2.
    """
    text = prepared.decode('utf-8')
    if not text.startswith('let main = fn() => (let ') or not text.rstrip().endswith(') in main()'):
        raise ValueError('invalid compiler context source format')
    depth = 0
    in_string = False
    escaped = False
    sites = {}
    for index, char in enumerate(text):
        if in_string:
            if escaped:
                escaped = False
            elif char == '\\':
                escaped = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
            continue
        if char == '(':
            depth += 1
        elif char == ')':
            depth -= 1
            if depth < 0:
                raise ValueError('unbalanced compiler context source')
        if depth == 1 and text.startswith(' in let ', index):
            rest = text[index + len(' in let '):]
            match = re.match(r'([a-zA-Z_][a-zA-Z0-9_]*) = fn\(', rest)
            if match:
                name = match.group(1)
                if name in sites:
                    raise ValueError(f'compiler context duplicate helper {name}')
                sites[name] = index
    if depth != 0 or in_string or 'instr' not in sites or boundary not in sites:
        raise ValueError(f'unknown or malformed compiler context boundary {boundary}')
    if sites[boundary] <= sites['instr']:
        raise ValueError('compiler context boundary precedes instr')
    return (text[:sites[boundary]] + ' in instr(82)(25)(7)(0)) in main()').encode('utf-8')


def verify_compiler_context(nulang: Path, stage2_path: Path, prepared: bytes,
                            work: Path, boundary: str) -> str:
    """Run strict emission and result comparisons on one compiler prefix."""
    source = compiler_context_source(prepared, boundary)
    host_hex = run([str(nulang), 'bootstrap/compile_hex.nula'], source)
    self_hex = run([str(nulang), str(stage2_path)], source)
    assert_emissions_equal('compiler_context', host_hex, self_hex)
    for generation, stream in (('stage1', host_hex), ('stage2', self_hex)):
        binary = work / f'compiler-context-{generation}.nbc'
        binary.write_bytes(assemble(stream))
        actual = last_result(run([str(nulang), str(binary)]).decode('utf-8'))
        if actual != '1377371904':
            raise AssertionError(
                f'{generation} expected 1377371904, got {actual!r}'
            )
    return 'parity 1377371904 in both generations'


def verify_context_sweep(nulang: Path, stage2_path: Path, prepared: bytes,
                         work: Path, boundaries: tuple[str, ...] =
                         ('emit_word', 'read_int', 'prec_of', 'comp_infix_call', 'compile')) -> str:
    """Report independent non-monotonic checkpoints; never accept a mismatch."""
    results = []
    for boundary in boundaries:
        try:
            outcome = verify_compiler_context(nulang, stage2_path, prepared, work, boundary)
        except (AssertionError, RuntimeError, ValueError, UnicodeError) as exc:
            outcome = str(exc)[:1100] or type(exc).__name__
        results.append(f'{boundary}: {outcome}')
    return ' | '.join(results)


def verify_compiler_prefix(nulang: Path, stage2_path: Path,
                           prepared: bytes, work: Path) -> str:
    """Check host/self-compiled parity for the exact leading compiler context."""
    source = compiler_prefix_source(prepared)
    host_hex = run([str(nulang), 'bootstrap/compile_hex.nula'], source)
    stage2_hex = run([str(nulang), str(stage2_path)], source)
    assert_emissions_equal('compiler_prefix', host_hex, stage2_hex)
    for name, emission in (('stage1', host_hex), ('stage2', stage2_hex)):
        binary = work / f'compiler_prefix-{name}.nbc'
        binary.write_bytes(assemble(emission))
        result = last_result(run([str(nulang), str(binary)]).decode('utf-8'))
        if result != '1377371904':
            raise AssertionError(
                f'compiler_prefix {name}: expected 1377371904, got {result!r}'
            )
    return 'compiler-prefix contextual parity: 1377371904 in both generations'


def verify_fixed_point(nulang: Path, prepared: bytes, stage2: bytes, work: Path,
                       stage1_hex: bytes | None = None) -> None:
    """Require the restricted compiler built by Stage 1 to reproduce its own artifact.

    The Rust-hosted VM still executes both generations, and Python still
    prepares/assembles the input. This does not prove host independence.
    """
    stage2_path = work / 'self_compile.nbc'
    stage2_path.write_bytes(stage2)
    stage2_hex = run([str(nulang), str(stage2_path)], prepared)
    stage3 = assemble(stage2_hex)
    if stage3 != stage2:
        first = hashlib.sha256(stage2).hexdigest()
        next_ = hashlib.sha256(stage3).hexdigest()
        byte_offset = first_difference(stage2, stage3)
        details = f'; first differing compiled byte at offset {byte_offset}'
        if stage1_hex is not None:
            host = instructions(stage1_hex.decode('utf-8'))
            self_hosted = instructions(stage2_hex.decode('utf-8'))
            hex_offset = first_difference(host, self_hosted)
            if hex_offset >= 0:
                word_start = hex_offset // 8 * 8
                details += (
                    f'; first differing emitted hex character at offset {hex_offset}'
                    f' (word {hex_offset // 8}); '
                    f'Stage 1 word={host[word_start:word_start + 8]!s}; '
                    f'Stage 2 word={self_hosted[word_start:word_start + 8]!s}; '
                    f'Stage 1 nearby: {emission_context(host, hex_offset // 8)}; '
                    f'Stage 2 nearby: {emission_context(self_hosted, hex_offset // 8)}; '
                    f'Stage 1 recent markers: '
                    f'{marker_context(stage1_hex.decode("utf-8"), hex_offset // 8)}; '
                    f'Stage 2 recent markers: '
                    f'{marker_context(stage2_hex.decode("utf-8"), hex_offset // 8)}'
                )
            else:
                details += '; normalized compiler emissions match (fixup/metadata divergence)'
        if stage1_hex is not None:
            try:
                sweep_result = verify_context_sweep(nulang, stage2_path, prepared, work)
                details += f'; compiler context sweep: {sweep_result}'
            except (AssertionError, RuntimeError, ValueError, UnicodeError) as exc:
                details += f'; compiler context sweep failed: {exc}'
        raise AssertionError(
            'bootstrap fixed-point mismatch: '
            f'Stage 2 {len(stage2)} bytes sha256={first}, '
            f'Stage 3 {len(stage3)} bytes sha256={next_}{details}'
        )
    print(f'PASS bootstrap fixed-point: {len(stage2)} byte-identical bytes', flush=True)


def verify(nulang: Path, corpus: Path, fixed_point: bool = False) -> None:
    cases = load_cases(corpus)
    # Stage 2 requires the Nulang emitter to compile its preprocessed source.
    # Only the already-built Rust host binary executes the bootstrap programs.
    prepared = run([sys.executable, 'bootstrap/prep_core.py'],
                   (ROOT / 'bootstrap/compile_hex.nula').read_bytes())
    stage1_hex = run([str(nulang), 'bootstrap/compile_hex.nula'], prepared)
    stage2 = assemble(stage1_hex)
    with tempfile.TemporaryDirectory(prefix='nulang-bootstrap-') as work:
        stage2_path = Path(work) / 'self_compile.nbc'
        stage2_path.write_bytes(stage2)
        for case in cases:
            source = case['source'].encode('utf-8')
            host_hex = run([str(nulang), 'bootstrap/compile_hex.nula'], source)
            self_hex = run([str(nulang), str(stage2_path)], source)
            assert_emissions_equal(case['id'], host_hex, self_hex)
            for generation, output in (('stage1', host_hex), ('stage2', self_hex)):
                artifact = Path(work) / f'{case["id"]}-{generation}.nbc'
                artifact.write_bytes(assemble(output))
                actual = last_result(run([str(nulang), str(artifact)]).decode('utf-8'))
                if actual != case['expected']:
                    raise AssertionError(
                        f'{case["id"]} ({generation}): expected {case["expected"]!r}, got {actual!r}'
                    )
            print(f'PASS {case["id"]}: both generations returned {case["expected"]}', flush=True)
        if fixed_point:
            verify_fixed_point(nulang, prepared, stage2, Path(work), stage1_hex=stage1_hex)
    print(f'PASS bootstrap differential corpus: {len(cases)} cases', flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--nulang', required=True, type=Path, help='path to built nulang binary')
    parser.add_argument('--corpus', type=Path, default=CORPUS)
    parser.add_argument('--fixed-point', action='store_true',
                        help='add the full restricted-compiler Stage 2 → Stage 3 byte-identity oracle')
    arguments = parser.parse_args()
    nulang = arguments.nulang.resolve()
    if not nulang.is_file():
        parser.error(f'nulang binary does not exist: {nulang}')
    os.environ.setdefault('NULANG_STEP_LIMIT', '400000000')
    try:
        verify(nulang, arguments.corpus, fixed_point=arguments.fixed_point)
    except (AssertionError, RuntimeError, ValueError, UnicodeDecodeError) as error:
        print(f'FAIL bootstrap differential: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())

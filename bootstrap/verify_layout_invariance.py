#!/usr/bin/env python3
"""Require bootstrap generation parity after deleting an unused source helper.

RED until the remaining layout-sensitive closure-capture defect is fixed.
Does not edit the checkout or weaken the normal Stage 2 -> Stage 3 oracle.
"""
import argparse
import os
from pathlib import Path
import sys
import tempfile

# Unit tests import this file directly, outside bootstrap's module path.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from verify_differential import (  # noqa: E402
    CORPUS, ROOT, assemble, assert_emissions_equal, last_result,
    load_cases, run, verify_fixed_point,
)


def without_unused_instr(source: str) -> str:
    """Delete precisely the unused instr definition; never rewrite expressions."""
    if source.count('fn instr(') != 1:
        raise ValueError('layout variant requires exactly one fn instr definition')
    if source.count('instr(') != 1:
        raise ValueError('cannot delete instr: still referenced outside its definition')
    start = source.index('fn instr(')
    end = source.find('fn emit_word(', start)
    if end < 0:
        raise ValueError('layout variant has end before instr or missing emit_word')
    block = source[start:end]
    if not block.rstrip().endswith('}'):
        raise ValueError('layout variant has malformed unused helper boundary')
    return source[:start] + source[end:]


def verify(nulang: Path) -> None:
    source = (ROOT / 'bootstrap' / 'compile_hex.nula').read_text(encoding='utf-8')
    variant = without_unused_instr(source)
    cases = load_cases(CORPUS)
    with tempfile.TemporaryDirectory(prefix='nulang-layout-') as workspace:
        work = Path(workspace)
        variant_path = work / 'compile_hex_without_instr.nula'
        variant_path.write_text(variant, encoding='utf-8')
        prepared = run([sys.executable, 'bootstrap/prep_core.py'], variant.encode('utf-8'))
        host_compiler_hex = run([str(nulang), str(variant_path)], prepared)
        stage2 = assemble(host_compiler_hex)
        stage2_path = work / 'self_compile.nbc'
        stage2_path.write_bytes(stage2)

        for case in cases:
            program = case['source'].encode('utf-8')
            host_hex = run([str(nulang), str(variant_path)], program)
            self_hex = run([str(nulang), str(stage2_path)], program)
            assert_emissions_equal('layout_' + case['id'], host_hex, self_hex)
            for generation, emission in (('stage1', host_hex), ('stage2', self_hex)):
                artifact = work / f'{case["id"]}-{generation}.nbc'
                artifact.write_bytes(assemble(emission))
                actual = last_result(run([str(nulang), str(artifact)]).decode('utf-8'))
                if actual != case['expected']:
                    raise AssertionError(
                        f'layout_{case["id"]} ({generation}): '
                        f'expected {case["expected"]!r}, got {actual!r}'
                    )
            print(f'PASS layout_{case["id"]}: {case["expected"]}', flush=True)

        verify_fixed_point(nulang, prepared, stage2, work, stage1_hex=host_compiler_hex)
    print('PASS bootstrap source-layout invariance (unused instr removed)', flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--nulang', type=Path, required=True)
    arguments = parser.parse_args()
    nulang = arguments.nulang.resolve()
    if not nulang.is_file():
        parser.error(f'nulang binary does not exist: {nulang}')
    os.environ.setdefault('NULANG_STEP_LIMIT', '400000000')
    try:
        verify(nulang)
    except (AssertionError, RuntimeError, ValueError, UnicodeDecodeError) as error:
        print(f'FAIL bootstrap source-layout invariance: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())

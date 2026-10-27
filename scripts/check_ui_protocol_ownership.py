#!/usr/bin/env python3
"""Keep renderer UI protocol Rust references inside src/web/ until extraction.

A lightweight dependency-direction ratchet, not a Rust parser or proof that
UI code is absent from the core. Comments are ignored; ambiguous references
outside the web boundary are deliberately rejected for review.
"""

import argparse
import pathlib
import re
import sys

IDENTIFIER = re.compile(r"\bnulang_ui_protocol\b")


def mask_comments(source):
    """Replace // and nested /* */ comments with spaces, preserving newlines."""
    out = list(source)
    i = 0
    size = len(source)
    while i < size:
        if source[i] == '"':
            i += 1
            while i < size:
                if source[i] == '\\':
                    i += 2
                elif source[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
        elif source.startswith('//', i):
            end = source.find('\n', i)
            if end < 0:
                end = size
            for p in range(i, end):
                out[p] = ' '
            i = end
        elif source.startswith('/*', i):
            depth = 1
            start = i
            i += 2
            while i < size and depth:
                if source.startswith('/*', i):
                    depth += 1
                    i += 2
                elif source.startswith('*/', i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError('Unterminated block comment in Rust source')
            for p in range(start, i):
                if out[p] != '\n':
                    out[p] = ' '
        else:
            i += 1
    return ''.join(out)


def find_violations(root):
    root = pathlib.Path(root)
    source = root / 'src'
    if not source.is_dir():
        raise ValueError('Missing src/ Rust source tree')
    paths = sorted(source.rglob('*.rs'))
    if not paths:
        raise ValueError('No Rust source files were found')
    violations = []
    for path in paths:
        relative = path.relative_to(root)
        if relative.parts[:2] == ('src', 'web'):
            continue
        code = mask_comments(path.read_text(encoding='utf-8'))
        for match in IDENTIFIER.finditer(code):
            line = code.count('\n', 0, match.start()) + 1
            violations.append(f'{relative.as_posix()}:{line}')
    return sorted(violations)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        violations = find_violations(args.root)
    except (OSError, UnicodeError, ValueError) as exc:
        print(f'ERROR: UI protocol ownership not verified: {exc}', file=sys.stderr)
        return 2
    if violations:
        print('ERROR: nulang_ui_protocol reference escaped src/web:', file=sys.stderr)
        for violation in violations:
            print(f'  {violation}', file=sys.stderr)
        return 1
    print('PASS: nulang_ui_protocol Rust references stay within src/web/')
    return 0


if __name__ == '__main__':
    sys.exit(main())

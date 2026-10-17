#!/usr/bin/env python3
"""Gate Nulang JSON warnings against a checked-in, count-aware baseline.

Usage:
    nulang --check src/main.nula --json > diagnostics.json
    python3 scripts/diagnostic_baseline.py --report diagnostics.json \
        --baseline diagnostics-baseline.json --update
    python3 scripts/diagnostic_baseline.py --report diagnostics.json \
        --baseline diagnostics-baseline.json

Only coded warnings are baselined. Compiler/type/effect/capability errors remain
fatal, even when --update is used. Stale entries fail normal checks so the
baseline is reduced when warnings are fixed. The basename is not tied to line
numbers, which allows source edits without hiding additional diagnostics.
"""

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import sys
import tempfile

SCHEMA_VERSION = 1


class InputError(ValueError):
    pass


def read_object(path):
    try:
        value = json.loads(path.read_text(encoding='utf-8'))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise InputError(f'{path}: {exc}') from exc
    if not isinstance(value, dict):
        raise InputError(f'{path}: expected a JSON object')
    return value


def normalize_file(name, root):
    if not isinstance(name, str) or not name:
        raise InputError('diagnostic requires a source filename')
    if name == '<input>':
        return name
    path = Path(name)
    root = root.resolve()
    if path.is_absolute():
        try:
            path = path.resolve().relative_to(root)
        except ValueError as exc:
            raise InputError(f'source outside --root: {name}') from exc
    if not path.parts or '..' in path.parts:
        raise InputError(f'unsafe source path: {name}')
    return path.as_posix()


def report_warnings(report, root):
    if report.get('schema_version') != SCHEMA_VERSION:
        raise InputError('unsupported diagnostic report schema_version')
    diagnostics = report.get('diagnostics')
    if not isinstance(diagnostics, list) or not isinstance(report.get('ok'), bool):
        raise InputError('report requires ok: boolean and diagnostics: array')
    warnings = Counter()
    errors = []
    for item in diagnostics:
        if not isinstance(item, dict) or item.get('severity') not in ('warning', 'error', 'note'):
            raise InputError('malformed diagnostic or unknown severity')
        severity = item['severity']
        if severity == 'error':
            errors.append(str(item.get('message', '<unknown error>')))
        if severity != 'warning':
            continue
        code = item.get('code')
        message = item.get('message')
        if not isinstance(code, str) or not code.strip():
            raise InputError('warnings require stable codes to be baselined')
        if not isinstance(message, str) or not message:
            raise InputError('warnings require non-empty messages')
        span = item.get('span')
        if span is not None and not isinstance(span, dict):
            raise InputError('malformed diagnostic span')
        filename = (span or {}).get('file') or report.get('file')
        warnings[(normalize_file(filename, root), code, message)] += 1
    # A failed compiler command without a diagnostic must not be interpreted
    # as a successful warning-only report.
    if not report['ok'] and not errors:
        errors.append('report ok=false without an error diagnostic')
    return warnings, errors


def read_baseline(path):
    if not path.exists():
        return Counter()
    data = read_object(path)
    if data.get('schema_version') != SCHEMA_VERSION or not isinstance(data.get('warnings'), list):
        raise InputError('unsupported baseline schema (expected v1 warnings array)')
    counts = Counter()
    for item in data['warnings']:
        if not isinstance(item, dict) or set(item) != {'file', 'code', 'message', 'count'}:
            raise InputError('invalid baseline warning record')
        file, code, msg, count = (item[k] for k in ('file', 'code', 'message', 'count'))
        if not all(isinstance(v, str) and v for v in (file, code, msg)) or type(count) is not int or count < 1:
            raise InputError('invalid baseline warning fields')
        key = (file, code, msg)
        if key in counts:
            raise InputError('duplicate baseline warning record')
        counts[key] = count
    return counts


def write_baseline(path, warnings):
    records = [dict(file=file, code=code, message=message, count=count)
               for (file, code, message), count in sorted(warnings.items())]
    data = json.dumps({'schema_version': SCHEMA_VERSION, 'warnings': records}, indent=2) + '\n'
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = None
    try:
        with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=path.parent,
                                         prefix=f'.{path.name}.', delete=False) as f:
            tmp = Path(f.name)
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    finally:
        if tmp is not None:
            tmp.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, required=True, help='nulang --json report')
    parser.add_argument('--baseline', type=Path, required=True, help='checked-in warning baseline')
    parser.add_argument('--root', type=Path, default=Path.cwd(), help='source root for relative paths')
    parser.add_argument('--update', action='store_true', help='explicitly rewrite warning baseline')
    args = parser.parse_args()
    try:
        actual, errors = report_warnings(read_object(args.report), args.root)
        if errors:
            for error in errors:
                print(f'compiler error (not baselinable): {error}', file=sys.stderr)
            return 1
        if args.update:
            write_baseline(args.baseline, actual)
            print(f'updated {args.baseline}: {sum(actual.values())} warnings')
            return 0
        expected = read_baseline(args.baseline)
    except InputError as exc:
        print(f'invalid diagnostic baseline input: {exc}', file=sys.stderr)
        return 2
    except OSError as exc:
        print(f'diagnostic baseline I/O failure: {exc}', file=sys.stderr)
        return 2

    added = actual - expected
    stale = expected - actual
    if added or stale:
        for label, diff in (('new', added), ('stale', stale)):
            for (file, code, message), count in sorted(diff.items()):
                print(f'{label} warning ({count}): {file} [{code}] {message}', file=sys.stderr)
        return 1
    print(f'diagnostic baseline matches: {sum(actual.values())} warnings')
    return 0


if __name__ == '__main__':
    sys.exit(main())

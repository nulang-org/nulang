#!/usr/bin/env python3
"""Fail closed on malformed GitHub issue forms and issue-template config.

Requires PyYAML 6.0.3. This intentionally checks a conservative subset of
GitHub's issue-form schema; it does not replace GitHub's own validation or the
docs build. Use it as a standalone, locally testable fast validation primitive.
"""

import argparse
from pathlib import Path
import re
import sys
from urllib.parse import urlparse

try:
    import yaml
except ImportError as exc:
    raise SystemExit("PyYAML is required: python3 -m pip install PyYAML==6.0.3") from exc


class StrictLoader(yaml.SafeLoader):
    """Reject duplicate keys and PyYAML's YAML 1.1 boolean aliases."""


# GitHub issue forms use true/false for flags. PyYAML's SafeLoader also treats
# yes/no/on/off as bool under YAML 1.1, which can silently accept typo-prone
# strings where GitHub expects a boolean. Override *only* this subclass's
# implicit resolvers without modifying the global SafeLoader.
StrictLoader.yaml_implicit_resolvers = {
    first: [(tag, regex) for tag, regex in resolvers if tag != 'tag:yaml.org,2002:bool']
    for first, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()
}
StrictLoader.add_implicit_resolver(
    'tag:yaml.org,2002:bool',
    re.compile(r'^(?:true|True|TRUE|false|False|FALSE)$'),
    list('tTfF'),
)


def _unique_mapping(loader, node):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=True)
        if not isinstance(key, (str, int, float, bool)):
            raise yaml.constructor.ConstructorError(
                None, None, "unhashable or non-scalar YAML mapping key", key_node.start_mark
            )
        if key in result:
            raise yaml.constructor.ConstructorError(
                None, None, f"duplicate YAML mapping key {key!r}", key_node.start_mark
            )
        result[key] = loader.construct_object(value_node, deep=True)
    return result


StrictLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, _unique_mapping)
CONTROL_TYPES = frozenset({'markdown', 'input', 'textarea', 'dropdown', 'checkboxes'})
CONTROL_ID = re.compile(r'^[a-zA-Z0-9_-]+$')


def _text(mapping, key, location, errors):
    value = mapping.get(key)
    if not isinstance(value, str) or not value.strip():
        errors.append(f"{location}: '{key}' must be non-empty text")
    return value


def _validate_form(data, location):
    errors = []
    if not isinstance(data, dict):
        return [f"{location}: issue form must be a YAML mapping"]
    _text(data, 'name', location, errors)
    _text(data, 'description', location, errors)
    body = data.get('body')
    if not isinstance(body, list) or not body:
        return errors + [f"{location}: 'body' must be a non-empty list"]
    seen_ids = set()
    for i, control in enumerate(body, start=1):
        prefix = f"{location}: body[{i}]"
        if not isinstance(control, dict):
            errors.append(f'{prefix}: control must be a mapping')
            continue
        kind = control.get('type')
        if not isinstance(kind, str) or kind not in CONTROL_TYPES:
            errors.append(f'{prefix}: unsupported control type {kind!r}')
            continue
        attributes = control.get('attributes')
        if not isinstance(attributes, dict):
            errors.append(f'{prefix}: attributes must be a mapping')
            continue
        if kind == 'markdown':
            _text(attributes, 'value', prefix, errors)
        else:
            identifier = _text(control, 'id', prefix, errors)
            if isinstance(identifier, str) and identifier.strip():
                if not CONTROL_ID.fullmatch(identifier):
                    errors.append(f'{prefix}: id must contain only letters, digits, _ or -')
                if identifier in seen_ids:
                    errors.append(f'{prefix}: duplicate control id {identifier!r}')
                seen_ids.add(identifier)
            _text(attributes, 'label', prefix, errors)
            if kind in ('dropdown', 'checkboxes'):
                options = attributes.get('options')
                if not isinstance(options, list) or not options:
                    errors.append(f'{prefix}: {kind} requires a non-empty options list')
        if 'validations' in control:
            validations = control['validations']
            if not isinstance(validations, dict):
                errors.append(f'{prefix}: validations must be a mapping')
            elif 'required' in validations and type(validations['required']) is not bool:
                errors.append(f'{prefix}: validations.required must be boolean')
    return errors


def _validate_config(data, location):
    errors = []
    if not isinstance(data, dict):
        return [f'{location}: issue-template config must be a YAML mapping']
    if 'blank_issues_enabled' in data and type(data['blank_issues_enabled']) is not bool:
        errors.append(f'{location}: blank_issues_enabled must be boolean')
    links = data.get('contact_links', [])
    if not isinstance(links, list):
        return errors + [f'{location}: contact_links must be a list']
    for i, link in enumerate(links, start=1):
        prefix = f'{location}: contact_links[{i}]'
        if not isinstance(link, dict):
            errors.append(f'{prefix}: contact link must be a mapping')
            continue
        _text(link, 'name', prefix, errors)
        _text(link, 'about', prefix, errors)
        url = _text(link, 'url', prefix, errors)
        if isinstance(url, str):
            try:
                parsed = urlparse(url)
                valid = parsed.scheme == 'https' and bool(parsed.hostname)
            except ValueError:  # e.g. an unmatched IPv6 bracket
                valid = False
            if not valid:
                errors.append(f'{prefix}: url must be an absolute https:// URL')
    return errors


def validate_templates(directory):
    directory = Path(directory)
    if not directory.is_dir() or directory.is_symlink():
        return [f'{directory}: issue-template directory missing or unsafe']
    paths = sorted(p for p in directory.iterdir() if p.suffix.lower() in ('.yml', '.yaml'))
    if not paths:
        return [f'{directory}: no GitHub issue templates found']
    errors = []
    for path in paths:
        if path.is_symlink() or not path.is_file():
            errors.append(f'{path}: issue template must be a regular file (not a symlink)')
            continue
        try:
            if path.stat().st_size > 1024 * 1024:
                errors.append(f'{path}: issue form exceeds 1 MiB')
                continue
            data = yaml.load(path.read_text(encoding='utf-8'), Loader=StrictLoader)
        except (OSError, UnicodeError, yaml.YAMLError, ValueError) as exc:
            errors.append(f'{path}: invalid YAML: {exc}')
            continue
        validator = _validate_config if path.stem == 'config' else _validate_form
        errors.extend(validator(data, str(path)))
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path.cwd())
    args = parser.parse_args(argv)
    errors = validate_templates(args.root / '.github' / 'ISSUE_TEMPLATE')
    for error in errors:
        print(f'ERROR {error}', file=sys.stderr)
    if errors:
        print(f'{len(errors)} issue template validation error(s)', file=sys.stderr)
        return 1
    print('GitHub issue templates: valid')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

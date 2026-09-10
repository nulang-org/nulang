#!/usr/bin/env python3
"""prep_core.py - Convert compiler_core.nula into a form compile_hex.nula can compile.

Transforms applied:
  1. Strip comments.
  2. Convert top-level `fn name(params) -> Type { body }` into a let-chain of
     single-parameter functions, wrapping everything in `main(...)`.
  3. Curry multi-argument call sites.
  4. Flatten `{ ... }` blocks into nested `let/in` or parenthesized expressions.
  5. Convert unsupported operators (`<<`, `>>`) to arithmetic.
  6. Emit the result as a single line (compile_hex.nula treats newlines as terminators).
"""

import sys
import re

def strip_comments(src: str) -> str:
    lines = []
    for line in src.splitlines():
        if '//' in line:
            line = line.split('//')[0]
        lines.append(line)
    return '\n'.join(lines)


def balanced_split_args(args_str: str) -> list[str]:
    args = []
    depth = 0
    curr = ''
    for ch in args_str:
        if ch == ',' and depth == 0:
            args.append(curr.strip())
            curr = ''
        else:
            if ch in '({[':
                depth += 1
            elif ch in ')}]':
                depth -= 1
            curr += ch
    if curr.strip():
        args.append(curr.strip())
    return args


def curry_call(name: str, args_str: str) -> str | None:
    args = balanced_split_args(args_str)
    if len(args) <= 1:
        return None
    result = name + '(' + args[0] + ')'
    for a in args[1:]:
        result += '(' + a + ')'
    return result


def transform_perform(source: str) -> str:
    """Transform `perform Effect.name(args)` into `_perform("Effect.name")(args)`."""
    result = []
    i = 0
    in_str = False
    while i < len(source):
        ch = source[i]
        if ch == '"':
            in_str = not in_str
            result.append(ch)
            i += 1
            continue
        if in_str:
            result.append(ch)
            i += 1
            continue
        m = re.match(r'\bperform\s+', source[i:])
        if m:
            j = i + m.end()
            name_match = re.match(r'([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)?)', source[j:])
            if name_match:
                effect_name = name_match.group(1)
                k = j + name_match.end()
                while k < len(source) and source[k].isspace():
                    k += 1
                if k < len(source) and source[k] == '(':
                    depth = 1
                    end = k + 1
                    inner_str = False
                    while end < len(source) and depth > 0:
                        if source[end] == '"':
                            inner_str = not inner_str
                        elif not inner_str:
                            if source[end] == '(':
                                depth += 1
                            elif source[end] == ')':
                                depth -= 1
                        end += 1
                    if depth == 0:
                        args_str = source[k + 1:end - 1]
                        args = balanced_split_args(args_str)
                        if args:
                            curried = f'nperform("{effect_name}", ' + ', '.join(args) + ')'
                        else:
                            curried = f'nperform("{effect_name}")'
                        result.append(curried)
                        i = end
                        continue
        result.append(ch)
        i += 1
    out = ''.join(result)
    # Recurse to transform nested perform calls inside effect arguments.
    if out != source:
        out = transform_perform(out)
    return out


def curry_call_sites(source: str) -> str:
    """Curry function calls with multiple arguments, but not `fn` definitions or nperform."""
    KEYWORDS = {'then', 'else', 'if', 'let', 'in', 'fn', 'and', 'or', 'not'}

    def find_call_end(source: str, start: int) -> int:
        """Return the index just past the matching `)` for `(` at start, or len(source)."""
        depth = 1
        i = start + 1
        in_str = False
        while i < len(source) and depth > 0:
            ch = source[i]
            if ch == '"':
                in_str = not in_str
            elif not in_str:
                if ch == '(':
                    depth += 1
                elif ch == ')':
                    depth -= 1
            i += 1
        return i

    result = []
    i = 0
    while i < len(source):
        m = re.match(r'\b([A-Za-z_][A-Za-z0-9_]*)\s*\(', source[i:])
        if not m:
            result.append(source[i])
            i += 1
            continue
        name = m.group(1)
        paren_start = i + m.end() - 1
        paren_end = find_call_end(source, paren_start)
        args_str = source[paren_start + 1:paren_end - 1]
        full = source[i:paren_end]
        before = source[max(0, i - 10):i]
        after = source[paren_end:paren_end + 20]
        # Recursively curry all arguments so inner multi-arg calls are also curried.
        args = balanced_split_args(args_str)
        curried_args = [curry_call_sites(a) for a in args]
        # Skip keywords.
        if name in KEYWORDS:
            result.append(name + '(' + ', '.join(curried_args) + ')')
        # nperform effect calls are parsed specially by compile_hex, but their
        # arguments may still contain multi-arg function calls that need currying.
        elif name == 'nperform':
            result.append('nperform(' + ', '.join(curried_args) + ')')
        # Skip qualified names (String.charAt) and function definitions.
        elif i > 0 and source[i - 1] == '.':
            result.append(full)
        elif re.search(r'\bfn\s+$', before) and re.match(r'\s*(?:=>|->|\{)', after):
            result.append(full)
        elif len(args) == 0:
            result.append(name + '()')
        elif len(args) == 1:
            result.append(name + '(' + curried_args[0] + ')')
        else:
            c = curry_call(name, args_str)
            result.append(c if c else full)
        i = paren_end
    return ''.join(result)


def strip_type_annotations(params_str: str) -> str:
    """From `x: Int, y: String` produce `x, y`."""
    args = balanced_split_args(params_str)
    stripped = []
    for a in args:
        a2 = a.split(':', 1)[0].strip()
        if a2:
            stripped.append(a2)
    return ', '.join(stripped)


def curry_fn_definition(name: str, params: list[str], body: str) -> str:
    """Convert multi-param definition into nested single-param lambdas.

    Example:
        name='add', params=['x','y'], body='x + y'
      ->
        add = fn(x) => fn(y) => x + y
    """
    import sys
    print(f'CURRY {name} params={params} slice={params[1:]}', file=sys.stderr)
    if not params:
        return f'{name} = fn() => {body}'
    result = body
    for i, p in enumerate(reversed(params[1:])):
        print(f'  loop i={i} p={p!r} result_before={result[:40]!r}', file=sys.stderr)
        result = f'fn({p}) => {result}'
    result = f'{name} = fn({params[0]}) => {result}'
    print(f'CURRY result {result[:80]}', file=sys.stderr)
    return result


def flatten_blocks(source: str) -> str:
    """Convert `{ a; b; expr }` blocks into nested `let/in` or `(expr)`."""
    # Repeatedly find outermost {} blocks and replace them.
    changed = True
    while changed:
        changed = False
        out = []
        i = 0
        while i < len(source):
            if source[i] == '{':
                depth = 1
                j = i + 1
                in_str = False
                while j < len(source) and depth > 0:
                    ch = source[j]
                    if ch == '"':
                        in_str = not in_str
                    elif not in_str:
                        if ch == '{':
                            depth += 1
                        elif ch == '}':
                            depth -= 1
                    j += 1
                inner = source[i + 1:j - 1].strip()
                replacement = _flatten_block_body(inner)
                out.append(replacement)
                i = j
                changed = True
            else:
                out.append(source[i])
                i += 1
        source = ''.join(out)
    return source


def _flatten_block_body(body: str) -> str:
    """Flatten a block body (semicolon/newline-separated statements) into an expression."""
    # Split at top-level semicolons or newlines.  Newlines inside a multi-line
    # `if ... then ... else ...` chain are continuations: any part that begins
    # with `else` is merged back onto the previous part.
    parts = []
    depth = 0
    in_str = False
    curr = ''
    for ch in body:
        if ch == '"':
            in_str = not in_str
            curr += ch
        elif in_str:
            curr += ch
        elif ch in '({[':
            depth += 1
            curr += ch
        elif ch in ')}]':
            depth -= 1
            curr += ch
        elif (ch == ';' or ch == '\n') and depth == 0:
            if curr.strip():
                parts.append(curr.strip())
            curr = ''
        else:
            curr += ch
    if curr.strip():
        parts.append(curr.strip())

    # Merge `else` continuations.
    merged = []
    for part in parts:
        if merged and re.match(r'else\b', part):
            merged[-1] = merged[-1] + ' ' + part
        else:
            merged.append(part)
    parts = merged

    # Merge parts that continue across newlines: a line ending with a binary
    # operator, comma, or opening delimiter clearly continues; a line starting
    # with a binary operator or `else` belongs to the previous line.
    CONT_END = re.compile(r'(^|.*)([\+\-\*/=<>!,]|\(|\{|\[)\s*$')
    CONT_START = re.compile(r'^([\+\-\*/=<>!,]|else\b)')
    changed = True
    while changed and len(parts) > 1:
        changed = False
        new_parts = []
        i = 0
        while i < len(parts):
            if i + 1 < len(parts) and (
                CONT_END.match(parts[i]) or CONT_START.match(parts[i + 1])
            ):
                new_parts.append(parts[i] + ' ' + parts[i + 1])
                i += 2
                changed = True
            else:
                new_parts.append(parts[i])
                i += 1
        parts = new_parts

    if not parts:
        return '()'
    if len(parts) == 1:
        return '(' + parts[0] + ')'

    # Wrap as nested let/in: let a = stmt1 in let b = stmt2 in ... in final_expr
    # Heuristic: the LAST part is the value; preceding parts are let bindings if they
    # are of the form `let x = ...`. Otherwise wrap each non-let as `let _ = expr in`.
    expr = parts[-1]
    for stmt in reversed(parts[:-1]):
        m = re.match(r'let\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)', stmt, re.DOTALL)
        if m:
            # The body splitter may leave a trailing ` in` token on the
            # binding's source line (e.g. `let x = e in\n y`). Strip it so
            # we don't emit the double `in in` that compile_hex.nula parses
            # as part of the expression and silently drops the rest.
            val = re.sub(r'\s+in\s*$', '', m.group(2), count=1)
            expr = f'let {m.group(1)} = {val} in {expr}'
        else:
            expr = f'let _ = {stmt} in {expr}'
    return '(' + expr + ')'


def convert_operators(source: str) -> str:
    """Replace operators/lexemes that compile_hex.nula does not support."""
    # Replace `x << n` with `x * (1 << n)` for literal n.
    # Operand x may be a parenthesized or simple expression.
    def shift_repl(m: re.Match) -> str:
        x = m.group(1).strip()
        n = int(m.group(2))
        return f'({x} * {1 << n})'
    # Match either a parenthesized expression or a single atom before <<.
    source = re.sub(r'(\([^()]*(?:\([^()]*\)[^()]*)*\)|\w+)\s*<<\s*(\d+)', shift_repl, source)
    # compile_hex.nula's read_int only handles decimal literals, so convert
    # hex literals to decimal.
    source = re.sub(r'\b0x([0-9a-fA-F]+)\b', lambda m: str(int(m.group(1), 16)), source)
    return source


def parse_top_level_fns(source: str) -> tuple[list[tuple[str, list[str], str]], str]:
    """Parse top-level `fn name(params) -> Type { body }` blocks."""
    fns = []
    rest = source.strip()
    pattern = re.compile(
        r'^\s*fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?:->\s*\w+\s*)?\{',
        re.MULTILINE
    )
    while True:
        m = pattern.search(rest)
        if not m:
            break
        name = m.group(1)
        params_raw = m.group(2)
        start_body = rest.find('{', m.end() - 1)
        if start_body < 0:
            break
        depth = 1
        i = start_body + 1
        while i < len(rest) and depth > 0:
            if rest[i] == '{':
                depth += 1
            elif rest[i] == '}':
                depth -= 1
            i += 1
        body = rest[start_body + 1:i - 1].strip()
        params = [p.strip().split(':', 1)[0].strip() for p in balanced_split_args(params_raw)]
        params = [p for p in params if p]
        fns.append((name, params, body))
        rest = rest[i:].strip()
    return fns, rest


def convert_to_let_chain(fns: list[tuple[str, list[str], str]], main_expr: str) -> str:
    """Wrap functions in nested let/in and curry definitions.

    If the final main expression is empty, invoke the `main` function.
    """
    main_expr = main_expr.strip()
    if not main_expr and fns:
        main_expr = f'{fns[-1][0]}()'
    expr = transform_perform(main_expr)
    expr = curry_call_sites(expr)
    for name, params, body in reversed(fns):
        body = flatten_blocks(body)
        body = _flatten_block_body(body)
        body = convert_operators(body)
        body = transform_perform(body)
        body = curry_call_sites(body)
        defn = curry_fn_definition(name, params, body)
        expr = f'let {defn} in {expr}'
    return expr


def main():
    source = sys.stdin.read()
    source = strip_comments(source)
    fns, main_expr = parse_top_level_fns(source)
    result = convert_to_let_chain(fns, main_expr)
    # Collapse whitespace to a single line (preserve necessary spaces around identifiers).
    result = re.sub(r'\s+', ' ', result).strip()
    print(result)


if __name__ == '__main__':
    main()

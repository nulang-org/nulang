# json-ext

JSON Schema-flavored validation and pretty-printing on top of the official
[`json`](../json) package. Official seed package (Experimental tier).

## Install

```bash
nula add json-ext   # brings in `json` as a dependency
```

## Usage

```nulang
import lib

fn main() {
  let doc = parse("{\"name\": \"ada\", \"age\": 36}")
  perform IO.print(pretty(doc))

  let schema = parse("{\"type\": \"object\", \"required\": [\"name\"]}")
  if validate(schema, doc) then {
    perform IO.print("valid!")
  } else {
    perform IO.print("invalid")
  }
}
```

## API

| Function | Description |
|----------|-------------|
| `pretty(v)` / `pretty_at(v, depth)` | Pretty-print with two-space indentation. |
| `type_of(v)` | `"null"`, `"bool"`, `"number"`, `"string"`, `"array"`, or `"object"`. |
| `json_eq(a, b)` | Structural equality (array/field order matters). |
| `array_eq(xs, ys)` / `fields_eq(xs, ys)` | Element-wise helpers used by `json_eq`. |
| `spaces(n)` | A string of `n` spaces. |
| `validate(schema, value)` | Validate against a schema (see below). |

### Schema keywords

Schemas are plain JSON values:

- `{"type": "object", "required": [...], "properties": {k: subschema}}`
- `{"type": "array", "items": subschema}`
- `{"type": "string" | "number" | "bool" | "null"}`
- `{"enum": [v1, v2, ...]}` — structural membership
- `{}` accepts everything; extra object fields are allowed.

## Tests

```bash
nula test
```

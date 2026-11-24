# json

JSON parsing and encoding for Nulang. Official seed package (Experimental
tier) — a standalone port of the stdlib `json` module, proving that stdlib
extractions into the package registry work end to end.

## Install

```bash
nula add json
```

## Usage

```nulang
import lib   // path as installed by `nula add`

fn main() {
  let v = parse("{\"a\": 1, \"b\": [true, null]}")
  perform IO.print(stringify(v))                  // {"a":1,"b":[true,null]}
  perform IO.print(get_string(v, "a", "?"))       // fallback for wrong type
  perform IO.print(perform Float.to_string(get_number(v, "a", 0.0)))
}
```

## API

| Function | Description |
|----------|-------------|
| `parse(json: String) -> JsonValue` | Legacy permissive parser; trailing content ignored, empty input → `JsonNull`. |
| `parse_strict(json) -> Result[JsonValue, JsonParseError]` | Validate a complete JSON document with fixed 64 KiB and depth-64 limits. |
| `parse_strict_limited(json, max_bytes, max_depth) -> Result[JsonValue, JsonParseError]` | Set smaller, caller-specific input and nesting budgets. |
| `stringify(value: JsonValue) -> String` | Compact encoder with full string escaping. |
| `get_string(obj, key, default)` | String field lookup with default. |
| `get_number(obj, key, default)` | Numeric (Float) field lookup with default. |
| `get_bool(obj, key, default)` | Boolean field lookup with default. |

`JsonValue` is an ADT: `JsonNull | JsonBool(Bool) | JsonNumber(Float) |
JsonString(String) | JsonArray([JsonValue]) | JsonObject([(String, JsonValue)])`.

Numbers are stored as `Float`; `stringify` emits `1` for `1.0`, matching
`Float.to_string`. Numbers above 2^53 may lose integer precision.

### Strict parsing for untrusted input

`parse` remains permissive for compatibility. Prefer `parse_strict` for external
JSON, and `parse_strict_limited` to enforce a tighter application-specific budget:

```nulang
match parse_strict_limited(input, 8192, 16) {
    Ok(value) => perform IO.print(stringify(value)),
    Error(e) => perform IO.print("invalid JSON at byte " + perform Int.to_string(e.offset)),
}
```

Limits never expand the default safety envelope: `max_bytes` must be 1–65536
(UTF-8 bytes) and `max_depth` must be 0–64 (root at depth zero). An invalid
limit returns `Error` before parsing. At depth zero, scalar values and empty
containers are accepted but nested values are not. The caller must still
validate application schemas; strict parsing alone does not validate fields.

## Tests

```bash
nula test
```

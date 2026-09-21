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
  let parsed = parse_strict("{\"a\": 1, \"b\": [true, null]}")
  let v = match parsed {
    Ok(value) => value,
    Error(err) => JsonNull,
  }
  perform IO.print(stringify(v))                  // {"a":1,"b":[true,null]}
  perform IO.print(get_string(v, "a", "?"))       // fallback for wrong type
  perform IO.print(perform Float.to_string(get_number(v, "a", 0.0)))
}
```

## API

| Function | Description |
|----------|-------------|
| `parse_strict(json: String) -> Result[JsonValue, JsonParseError]` | Strict full-document parser; rejects malformed/truncated/trailing content. |
| `parse(json: String) -> JsonValue` | Legacy lenient parser retained during migration; trailing content ignored and empty input becomes `JsonNull`. |
| `stringify(value: JsonValue) -> String` | Compact encoder with full string escaping. |
| `get_string(obj, key, default)` | String field lookup with default. |
| `get_number(obj, key, default)` | Numeric (Float) field lookup with default. |
| `get_bool(obj, key, default)` | Boolean field lookup with default. |

`JsonValue` is an ADT: `JsonNull | JsonBool(Bool) | JsonNumber(Float) |
JsonString(String) | JsonArray([JsonValue]) | JsonObject([(String, JsonValue)])`.

Numbers are stored as `Float`; `stringify` emits `1` for `1.0`, matching
`Float.to_string`.

`parse_strict` currently validates and then delegates decoding to the legacy
parser, so it performs two linear passes. This is a transitional implementation
chosen to add correct failure semantics without breaking existing `parse`
callers; the passes should be consolidated when `parse` itself migrates to
`Result`.

## Tests

```bash
nula test
```

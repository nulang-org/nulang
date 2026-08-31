# result-ext

Combinators for `Option`/`Result` pipelines, extending the stdlib `option`
and `result` modules. Official seed package (Experimental tier).

## Install

```bash
nula add result-ext
```

## Usage

```nulang
import lib

fn main() {
  let parse_positive = fn(x) {
    if x > 0 then { Ok(x) } else { Error("not positive") }
  }

  // Chain and collect fallible computations.
  match traverse(parse_positive, [1, 2, 3]) {
    Ok(xs) => perform IO.print("all positive"),
    Error(e) => perform IO.print("failed: " + e),
  }

  // Combine two Results.
  let total = map2(fn(a, b) { a + b }, Ok(2), Ok(3))   // Ok(5)
}
```

## API

Result: `and_then`, `or_else`, `map_err`, `map2`, `unwrap_or`, `sequence`,
`traverse`.
Interop: `ok_or`, `to_option`.
Option: `option_and_then`, `option_map2`, `option_sequence`,
`option_traverse`, `option_unwrap_or`.

`sequence`/`traverse` return the first `Error` encountered; the Option
variants return `None` on the first `None`.

## Tests

```bash
nula test
```

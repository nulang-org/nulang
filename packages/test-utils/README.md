# test-utils

Assertions, fixtures, and helpers beyond the built-in `Test` effect and the
stdlib `test` module. Official seed package (Experimental tier).

## Install

```bash
nula add test-utils
```

## Usage

```nulang
import lib

fn main() {
  expect_eq_int(2 + 2, 4)
  expect_contains("hello world", "lo wo")
  expect_some(Some(1))

  // Table-driven tests via for_each.
  let cases = [(1, 1), (2, 4), (3, 9)]
  for_each(cases, fn(c) { expect_eq_int(c.0 * c.0, c.1) })
}
```

Every `expect_*` helper aborts the test with a descriptive runtime error on
failure, so `nula test` reports the file as FAILED.

## API

- Booleans: `expect_true`, `expect_false`
- Equality: `expect_eq_int`, `expect_ne_int`, `expect_eq_str`,
  `expect_eq_bool`, `expect_float_near` (epsilon), `expect_eq_list_int`
- Ordering: `expect_gt`, `expect_lt`, `expect_between`
- Option/Result: `expect_some`, `expect_none`, `expect_some_value`,
  `expect_ok`, `expect_err`
- Strings: `expect_contains`
- Fixtures: `for_each(cases, f)`, `unreachable(msg)`

## Tests

```bash
nula test
```

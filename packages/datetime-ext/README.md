# datetime-ext

Parsing, formatting, arithmetic, and calendar helpers extending the stdlib
`datetime` module. Official seed package (Experimental tier).

## Install

```bash
nula add datetime-ext
```

## Usage

```nulang
import lib

fn main() {
  let dt = from_iso("2024-02-28T12:30:00")
  perform IO.print(to_iso(add_days(dt, 1)))       // 2024-02-29T12:30:00
  perform IO.print(perform Int.to_string(day_of_week(dt)))  // 3 (Wednesday)
}
```

## API

- Calendar: `is_leap_year(y)`, `days_in_month(y, m)`, `day_of_week(dt)`
  (0 = Sunday)
- Arithmetic: `add_days(dt, n)`, `days_between(a, b)`, `compare(a, b)`
  (-1/0/1)
- Formatting: `pad_int(x, width)`, `to_iso_date(dt)` → `YYYY-MM-DD`,
  `to_iso(dt)` → `YYYY-MM-DDThh:mm:ss`
- Parsing: `from_iso(s)` (accepts `T` or space separator; missing time
  fields default to 0), `from_iso_date(s)`
- Conversion primitives: `days_from_civil(y, m, d)`,
  `civil_from_days(z) -> (Int, Int, Int)` — Howard Hinnant's algorithms,
  proleptic Gregorian calendar, years >= 1.

All of stdlib `datetime` (`now`, `new`, `is_valid`) is re-exported.

## Tests

```bash
nula test
```

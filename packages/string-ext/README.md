# string-ext

String helpers beyond the stdlib `string` module: slugify, truncate,
padding, case helpers, and `{{key}}` template interpolation. Official seed
package (Experimental tier).

## Install

```bash
nula add string-ext
```

## Usage

```nulang
import lib

fn main() {
  perform IO.print(slugify("Hello, World!"))                        // hello-world
  perform IO.print(truncate("a very long sentence", 10))            // a very ...
  perform IO.print(pad_left("7", 3, "0"))                           // 007
  perform IO.print(template("Hi {{who}}!", [("who", "ada")]))       // Hi ada!
}
```

## API

| Function | Description |
|----------|-------------|
| `slugify(s)` | URL-safe slug: lowercase, alnum runs, dashes between. |
| `truncate(s, max)` | Cut to `max` chars, appending `...` when cut. |
| `pad_left(s, width, pad)` / `pad_right(...)` / `center(...)` | Pad to width. |
| `capitalize(s)` | Uppercase the first ASCII letter. |
| `is_numeric(s)` | True when every char is an ASCII digit (non-empty). |
| `template(tmpl, vars)` | Replace `{{key}}` from `[(key, value)]`; unknown keys left as-is. |
| `indent_lines(s, prefix)` | Prefix every line. |

All of stdlib `string` (`split`, `join`, `replace`, `trim`, `to_upper`, ...)
is re-exported.

## Tests

```bash
nula test
```

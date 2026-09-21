# Nulang Release Checklist

> Run this checklist before every release. The crate version (`Cargo.toml`
> `version`) is the **implementation version** (semver). The language version
> (`[package.metadata] language-version` and `LANGUAGE_VERSION` in
> `src/format/constants.rs`) is the **language version** — it moves only on
> RFC-ratified change. See `GOVERNANCE.md` §5 for the distinction.

## Pre-flight

- [ ] `cargo test --lib` — all tests pass (1541+)
- [ ] `cargo check --tests` — zero warnings
- [ ] `./conformance/run.py` — conformance suite passes
- [ ] `cargo fmt -- --check` — no formatting diffs

## Examples

- [ ] All 17 numbered examples compile and run:
  ```
  for f in examples/[0-1][0-9]_*.nula; do
    echo "=== $f ===" && nulang "$f" && echo "OK: $f" || echo "FAIL: $f"
  done
  ```

## Templates

- [ ] All 4 templates scaffold, build, test, and run:
  ```
  for t in default cli lib full; do
    rm -rf /tmp/nula_test_$t
    nulang nula new /tmp/nula_test_$t --template $t
    cd /tmp/nula_test_$t
    nulang nula build
    nulang nula test
    nulang nula run
    cd -
  done
  ```

## Versioning

- [ ] `nulang --version` shows correct crate version (`0.1.0`)
- [ ] `nulang --help` output is current and accurate
- [ ] `Cargo.toml` `version` matches `src/main.rs` `VERSION` constant
- [ ] `Cargo.toml` `[package.metadata] language-version` matches
      `src/format/constants.rs` `LANGUAGE_VERSION`

## Documentation

- [ ] `CHANGELOG.md` includes all user-visible changes since the last release
- [ ] `README.md` links are valid (GitHub, website, docs)
- [ ] `examples/README.md` is up to date with the current example count
- [ ] README's platform list matches `.github/workflows/release.yml`

## Post-publish artifact verification

After the tag workflow publishes the release:

- [ ] Linux x86_64 archive + `.sha256` are present
- [ ] Linux aarch64 archive + `.sha256` are present
- [ ] macOS aarch64 archive + `.sha256` are present
- [ ] Windows x86_64 archive + `.sha256` are present
- [ ] Every published checksum verifies against its archive
- [ ] Extract each native-host archive and run `nulang --version`
- [ ] Follow the README install commands from a clean environment rather than
      relying on a maintainer checkout

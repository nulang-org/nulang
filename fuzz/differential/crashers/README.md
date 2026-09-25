# Differential fuzzing crashers

The 2,796 crasher inputs from the differential fuzzing campaign are **not**
checked into this repository. They are fully regenerable: the grammar-based
generator is deterministic per seed, so every crasher can be reproduced by
re-running the campaign.

## Regenerating

```sh
scripts/difffuzz.sh
```

or directly:

```sh
cargo run --release --locked --no-default-features --features difffuzz --bin nula_difffuzz -- --seeds 22400 --seed-base 0
cargo run --release --locked --no-default-features --features difffuzz --bin nula_difffuzz -- --seeds 8000 --seed-base 100000
```

The seed ranges used in the historical campaign are **0..22400** and
**100000..108000**, as documented in `docs/DIFFERENTIAL_FUZZING.md`. New
crashers are written directly under `fuzz/differential/crashers/` with
filenames of the form `seed_<16-hex-digit-seed>.nula`. Legacy subdirectories
such as `known-overflow/` describe historical campaign triage only; current
campaigns do not exempt any divergence class.

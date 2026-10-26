# Nulang Conformance Suite

This directory is the independent, reference behavioral specification for Nulang.
It is an artifact of the language, not the implementation: any future runtime
(e.g., the bootstrap compiler) must pass these cases to be considered a
conforming Nulang implementation.

- `grammar/`: Syntax positive/negative cases (run against parser directly).
- `behavior/`: End-to-end execution cases (`.nula` + expected `.json` output).

Run with `./run.py`.

Cases that deliberately exercise gated effects can specify `"capabilities": ["fs"]`
in their expected-output JSON. `run.py` forwards the listed grants via `--with`.
Cases without this field receive no additional resource capabilities.

### Durable-store isolation

Each behavioral fixture receives its own temporary durable-store directory,
which is deleted after the suite finishes. This prevents durable actor tests
from sharing `.nulang/store` across cases or successive suite runs. Fixtures
execute in sorted order for reproducible failure reports. Permissions are
unchanged: only explicit per-case `capabilities` grants enable fs, net or os.

Unit tests for the runner (without a Cargo build):
`python3 -m unittest conformance.test_run -v`.

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

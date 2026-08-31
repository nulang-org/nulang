# Push Instructions

This document records how to push the current working tree to the GitHub remote.

## Quick Push

```bash
cd /home/dporkka/dev/nulang
git push origin main
```

## Current State

- Branch: `main`
- Remote: `https://github.com/nulang-org/nulang.git`
- Build: `cargo build` succeeds
- Tests: `cargo test` succeeds (508 unit tests pass)

## Recent Changes

- Fixed upstream bytecode ISA / `Value` representation refactor that left the build broken.
- Restored `Runtime` and `fresh_actor_id()`.
- Replaced the colliding low-nibble NaN-boxing scheme with distinct high-16 type tags.
- Fixed VM operand layouts for constants, `Not`, `INeg`, jumps, `Load`/`Store`/`Dup`.
- Added lexer/parser support for `and`, `or`, `not`, `then`, `nil`, `case`.
- Implemented minimal array opcodes and capture-free closure support.
- Updated Python marshaling to use the new `Value` tagging.
- All unit tests now pass.

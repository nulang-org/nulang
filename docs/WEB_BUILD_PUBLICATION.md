# Static Web build publication (experimental)

`nula build --web` builds into an isolated sibling staging directory before
publishing the output configured by `[web].output_dir`. This supplements the
compiler-validated Web route contracts and the static-only rendering policy of
PRs #1734 and #1738.

## Successful builds

A successful build writes HTML, static assets, the client micro-runtime, signal
metadata, and `nulang-app.ir.json` into staging. It runs configured JavaScript
budgets before publication, then replaces the previous output tree with the
complete staged tree. Files removed from the source no longer survive in the
published directory.

The final output path remains unchanged (typically `dist/`). Deployment
adapters can continue reading it after `nula build --web` returns successfully.

## Failed builds

Compile, route-validation, prerender, asset-copy, IR-write, and budget failures
leave the previous output untouched. When no output exists yet, a failed first
build leaves the configured target absent. The staging directory is cleaned up
on normal error unwinding. If installing the new directory fails after the old
directory has been renamed aside, the publisher attempts to restore the old
directory and returns an error.

To avoid destructive first-time publication over unrelated directories, a
nonempty existing output must contain the `nulang-app.ir.json` marker from a
previous Nulang Web build. The two files emitted by `nula dev`
(`app.client.js` and `app.signals.json`) are recognized as a safe
pre-build output when no other files are present. Other nonempty directories
are rejected; choose a dedicated build path or explicitly migrate its
contents rather than treating arbitrary files as generated output.

Concurrent `nula build --web` invocations for the same output path are
rejected using an exclusive sibling lock file. The configured output must be
a normal relative path and cannot overlap `src`, `.nula`, `.git`, or the
configured source asset directory. Symlinked output paths and parents are
rejected. Static route artifacts cannot contain parent-directory traversal.

## Crash and deployment limitations

**This is not a fully crash-atomic switch.** Replacing a nonempty directory
takes two same-parent renames on supported filesystems; readers may briefly see
the output missing. A process kill, power loss, or disk failure between renames
can leave the prior site under a sibling directory with the name pattern
`.<output>.nulang-previous-<pid>-<id>`. It can also leave
`.<output>.nulang-stage-<pid>-<id>` and the
`.<output>.nulang-build.lock` file.

On unexpected termination, inspect these files and directories before
removing the lock. If the published output is missing and a previous-output
directory exists, move that preserved directory back into place **only after
ensuring no build is running**. Do not automatically delete possible recovery
artifacts or bypass a stale lock.

This change does not make `nula dev` an atomic publisher, does not isolate
arbitrary build-time host effects, and does not introduce cross-process
coordination beyond the static builder's lock. A deployment platform requiring
zero-downtime cutover should publish versioned directories and switch an
external pointer only after validation.

## Validation

Run against the exact PR head:

```bash
cargo fmt --all -- --check
cargo test --locked --lib package::web_output::tests
cargo test --locked --lib test_static_route_output_cannot_escape_staging_root
cargo test --locked --lib test_cmd_build_web_failed_budget_preserves_published_site
cargo test --locked --lib test_cmd_build_web_success_removes_obsolete_published_files
cargo test --locked --lib test_cmd_build_web_does_not_render_server_routes_as_static_artifacts
cargo test --locked --lib test_cmd_build_web_signal_hydration
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
```

The required full CI suite and platform-specific output replacement smoke tests
remain promotion gates; successful local tests do not substitute for them.

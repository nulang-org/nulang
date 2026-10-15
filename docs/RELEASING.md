# Cutting a Release

How to publish a Nulang release with prebuilt binaries. Run
[RELEASE_CHECKLIST.md](RELEASE_CHECKLIST.md) first.

## 1. Bump the version

- Update `version` in `Cargo.toml` and the `VERSION` constant in
  `src/main.rs` so they match (the checklist verifies this).
- The **language version** (`[package.metadata] language-version`) only moves
  on RFC-ratified changes — usually leave it alone. See `GOVERNANCE.md` §5.
- Add a `CHANGELOG.md` entry for the new version.

## 2. Validate the release candidate before tagging

The same four-target `.github/workflows/release.yml` matrix runs as a
**non-publishing rehearsal** when a pull request changes the release workflow,
`Cargo.toml`, `Cargo.lock`, `src/main.rs`, or release verification scripts.
Confirm the `Release` PR workflow reaches terminal success for all targets,
including artifact packaging, checksums, and native-on-runner smoke tests.
On pull requests, the version-tag check is skipped (there is no tag) and the
`Publish Release` job is explicitly disabled. A queued/skipped build is not
positive execution evidence. This does **not** replace clean-machine checks,
particularly native Linux arm64 execution.

## 3. Tag and push

Only after the release candidate passes the checklist and all required reviews,
push the chosen version tag. Tags matching `v*` trigger the publication path
in `.github/workflows/release.yml`:

```bash
git tag v0.2.0
git push origin v0.2.0
```

## 4. Watch the workflow

The `Release` workflow first verifies that the pushed tag matches both
`Cargo.toml` and the CLI `VERSION`, then builds four targets in parallel
(`linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `windows-x86_64`), using
`--no-default-features --features release-cli`. This distribution includes
the standard non-Python CLI functionality (native compiler/runtime, LSP,
SQLite-backed storage, network/TLS/AI runtime and FFI) but **not Python
interop**, so its Linux ELF does not require the build runner's exact Python
shared-library ABI. Native Linux CI runs the CLI smoke test and native release
tests with the same profile. The `publish` job
creates a GitHub Release with:

- one `nulang-<platform>-<arch>.tar.gz` archive per supported target.
  The **member inside the tarball** is platform-qualified too (for example,
  `nulang-linux-x86_64`). Rename the extracted executable to `nulang`
  on Unix, or `nulang.exe` on Windows, before using normal CLI commands.
- a `.sha256` checksum per tarball
- auto-generated release notes

> Compatibility profile: `Cargo.toml` retains `full` as the source-build
> default, including Python interoperability. Developers who need Python
> integration can explicitly build it with:
>
> ```bash
> cargo build --locked --release --no-default-features --features full
> ```
>
> That build may require a compatible system Python development library.
> The prebuilt `release-cli` profile does not include Python interoperability.
> The Linux aarch64 archive is cross-compiled without PyO3. Do **not** claim
> that a cross-compiled binary has passed a native arm64 smoke test unless
> such a check actually ran.

## 5. Verify the release

- Open https://github.com/nulang-org/nulang/releases and confirm all 8 files
  (4 tarballs + 4 checksums) are attached.
- Smoke-test one artifact:

  ```bash
  # Replace TAG with the actual published version (after release).
  TAG="v0.2.0"
  BASE="https://github.com/nulang-org/nulang/releases/download/$TAG"
  curl -fL -O "$BASE/nulang-linux-x86_64.tar.gz"
  curl -fL -O "$BASE/nulang-linux-x86_64.tar.gz.sha256"
  sha256sum -c nulang-linux-x86_64.tar.gz.sha256
  tar xzf nulang-linux-x86_64.tar.gz
  mv nulang-linux-x86_64 nulang
  chmod +x nulang
  ./nulang --version   # must print the version just tagged
  ./nulang --eval 'perform IO.print("release works")'
  ```

- On Linux, verify dynamic dependencies and run the same smoke guard used by CI:
  ```bash
  bash scripts/check_release_linux.sh ./nulang
  ```
  This rejects `libpython*.so` ELF dependencies and missing shared libraries.
  It does not certify portability to every Linux distribution or CPU.
- Verify the extracted executables on clean supported Linux, macOS and Windows
  machines, **not only on their build runners**. In particular, require a
  Python-3.13-only or Python-absent Linux environment so the old Python-3.12
  linkage defect cannot be hidden by CI-installed libraries.
- If anything is wrong, stop publication or follow the documented release/tag
  correction process. Do not silently replace a published checksum or archive.

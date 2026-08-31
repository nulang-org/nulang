# Cutting a Release

How to publish a Nulang release with prebuilt binaries. Run
[RELEASE_CHECKLIST.md](RELEASE_CHECKLIST.md) first.

## 1. Bump the version

- Update `version` in `Cargo.toml` and the `VERSION` constant in
  `src/main.rs` so they match (the checklist verifies this).
- The **language version** (`[package.metadata] language-version`) only moves
  on RFC-ratified changes — usually leave it alone. See `GOVERNANCE.md` §5.
- Add a `CHANGELOG.md` entry for the new version.

## 2. Tag and push

Tags matching `v*` trigger `.github/workflows/release.yml`:

```bash
git tag v0.2.0
git push origin v0.2.0
```

## 3. Watch the workflow

The `Release` workflow builds four targets in parallel
(`linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`), runs
`cargo test --release` on the native Linux build, then the `publish` job
creates a GitHub Release with:

- `nulang-{linux,macos}-{x86_64,aarch64}.tar.gz` — binary inside is named
  `nulang`, matching the README install steps
- a `.sha256` checksum per tarball
- auto-generated release notes

> Note: the `linux-aarch64` artifact is cross-compiled and built **without**
> the default `python` feature (PyO3 cannot cross-link libpython). All other
> artifacts ship with default features.

## 4. Verify the release

- Open https://github.com/nulang-org/nulang/releases and confirm all 8 files
  (4 tarballs + 4 checksums) are attached.
- Smoke-test one artifact:

  ```bash
  curl -LO https://github.com/nulang-org/nulang/releases/download/v0.2.0/nulang-linux-x86_64.tar.gz
  sha256sum -c nulang-linux-x86_64.tar.gz.sha256
  tar xzf nulang-linux-x86_64.tar.gz
  ./nulang --version   # must print the version you just tagged
  ./nulang --eval 'perform IO.print("release works")'
  ```

- If anything is wrong, delete the release and tag (`git push origin :v0.2.0`
  plus the GitHub release page), fix, and re-tag.

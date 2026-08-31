# Publishing the Nulang VS Code extension

The extension publishes to two marketplaces:

- **VS Code Marketplace** (code.visualstudio.com) — requires an Azure DevOps
  publisher and a Personal Access Token.
- **Open VSX** (open-vsx.org) — required for VSCodium, Cursor, and GitLab's
  VS Code-based Web IDE. Requires an Open VSX namespace and token.

## One-time setup

### VS Code Marketplace

1. Create an Azure DevOps organization (any name, e.g. `nulang-org`).
2. Create a publisher named `nulang`:
   https://marketplace.visualstudio.com/manage → *New publisher*.
   The publisher name must match the `publisher` field in `package.json`.
3. Create a PAT in Azure DevOps with the **Marketplace > Manage** scope.
4. Add the PAT as the `VSCE_PAT` repository secret (GitHub →
   Settings → Secrets and variables → Actions).

### Open VSX

1. Create an Open VSX account and request the `nulang` namespace:
   https://open-vsx.org → *Publish extensions*.
2. Create an access token with the `publish` scope.
3. Add it as the `OVSX_TOKEN` repository secret.

## Publishing a release

1. Bump `version` in `package.json` and add a `CHANGELOG.md` entry.
2. Commit and push to `main`.
3. Tag the release and push the tag:

   ```sh
   git tag ext-v0.2.0
   git push origin ext-v0.2.0
   ```

   The `ext-v*` prefix is deliberate: it triggers the publish job in
   `.github/workflows/vscode-extension.yml` without colliding with the
   Rust release workflow's `v*` tags.

4. The workflow builds the `.vsix`, runs the integration tests, and
   publishes to both marketplaces (`--skip-duplicate` makes re-runs safe).

## Manual publishing (from a checkout)

```sh
cd editors/vscode
npm install
npm run compile
npx @vscode/vsce package            # -> nulang-<version>.vsix
npx @vscode/vsce publish -p "$VSCE_PAT" --packagePath nulang-<version>.vsix
npx ovsx publish nulang-<version>.vsix -p "$OVSX_TOKEN"
```

## Local verification before publishing

```sh
cd editors/vscode
npm install
npm run compile
NULANG_PATH=/path/to/nulang xvfb-run -a npm test   # integration tests
```

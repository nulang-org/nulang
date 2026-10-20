# Importing generated documentation assets

The Docs Sync GitHub Actions workflow regenerates the standard-library reference,
API reference, and browser-playground bundle, then verifies the Astro build.

The repository's organization-wide branch rules require reviewed pull requests
and reject direct/force pushes to the former `automation/docs-sync` branch.
To avoid hiding a successful generation behind a rejected push, the workflow
archives generated outputs for 30 days and emits a warning if they differ from
`main`. **A successful Docs Sync job does not mean generated outputs
have been merged or deployed.**

## Import an artifact through review

1. Open the latest successful **Docs Sync** run on GitHub. Download its
   `nulang-generated-docs` artifact, or use GitHub CLI:
   
   ```sh
   gh run download RUN_ID --repo nulang-org/nulang \
     --name nulang-generated-docs --dir docs
   ```

   The archive stores paths relative to `docs/`; use a clean local
   checkout when extracting it.
2. Create a working branch from the **same commit** that generated the artifact
   when possible, or regenerate on current `main` to avoid stale output.
3. Inspect the diffs under `docs/src/content/docs/stdlib/`,
   `docs/api.md`, and `docs/public/playground/`.
4. Validate the site and open a normal PR. Do not force-push a protected branch
   or silently rewrite generated artifacts.

Longer term, dedicated GitHub App credentials with an explicitly approved
repository-rule bypass could restore automatic PR proposals. Until then,
generated artifacts and review remain the fail-safe publication path.

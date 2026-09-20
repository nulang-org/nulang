#!/usr/bin/env node
/**
 * generate-docs.js — Auto-generate Nulang standard library documentation.
 *
 * Runs the Nulang compiler to extract built-in effect operation docs from
 * src/stdlib.rs and writes per-effect Markdown pages into the Starlight
 * docs content directory. Also regenerates the full `docs/api.md` reference.
 *
 * Prerequisites:
 *   - Rust toolchain (cargo)
 *   - Run from the repository root
 *
 * Usage:
 *   node scripts/generate-docs.js
 */

import { execSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, '..');
const STDLIB_DOCS_DIR = 'docs/src/content/docs/stdlib';

function run(cmd, opts = {}) {
  console.log(`  $ ${cmd}`);
  execSync(cmd, {
    stdio: 'inherit',
    cwd: REPO_ROOT,
    ...opts,
  });
}

function main() {
  console.log('=== Generating Nulang Standard Library Docs ===\n');

  // Step 1: Generate per-effect stdlib Markdown docs
  console.log('[1/2] Extracting built-in effect operations from src/stdlib.rs...');
  run(`cargo run --bin nulang -- --emit-stdlib-docs ${STDLIB_DOCS_DIR}`);

  // Step 2: Regenerate the full API reference (docs/api.md)
  console.log('\n[2/2] Regenerating full API reference...');
  run('cargo run --bin nulang -- --doc');

  console.log('\n=== Docs generated successfully ===');
  console.log(`  Stdlib pages: ${STDLIB_DOCS_DIR}/`);
  console.log('  API reference: docs/api.md');
}

main();

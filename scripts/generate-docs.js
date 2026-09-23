#!/usr/bin/env node
/**
 * generate-docs.js — Auto-generate Nulang standard library documentation.
 *
 * First verifies the canonical stdlib manifest and generated package/module
 * artifacts, then runs the Nulang compiler to emit built-in effect docs and
 * the full `docs/api.md` reference.
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

  // Step 1: Verify generated stdlib metadata/package mirrors before docs are
  // derived from them. This fails closed on drift instead of silently
  // publishing stale API documentation.
  console.log('[1/3] Verifying canonical stdlib manifest...');
  run('python3 scripts/generate_stdlib.py --check');

  // Step 2: Generate per-effect stdlib Markdown docs.
  console.log('\n[2/3] Extracting built-in effect operations...');
  run(`cargo run --bin nulang -- --emit-stdlib-docs ${STDLIB_DOCS_DIR}`);

  // Step 3: Regenerate the full API reference (docs/api.md)
  console.log('\n[3/3] Regenerating full API reference...');
  run('cargo run --bin nulang -- --doc');

  console.log('\n=== Docs generated successfully ===');
  console.log(`  Stdlib pages: ${STDLIB_DOCS_DIR}/`);
  console.log('  API reference: docs/api.md');
}

main();

import { access, copyFile, mkdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const docsRoot = path.resolve(here, '..');
const repoRoot = path.resolve(docsRoot, '..');
const sourceDir = path.join(repoRoot, 'playground', 'web');
const targetDir = path.join(docsRoot, 'public', 'playground');

await mkdir(targetDir, { recursive: true });

for (const file of ['index.html', 'playground.js', 'style.css']) {
  await copyFile(path.join(sourceDir, file), path.join(targetDir, file));
}

const wasm = 'nulang_playground.wasm';
const sourceWasm = path.join(sourceDir, wasm);
const targetWasm = path.join(targetDir, wasm);

try {
  await access(sourceWasm);
  await copyFile(sourceWasm, targetWasm);
  console.log('Synced browser playground, including WASM compiler.');
} catch {
  // Local/Cloudflare docs builds do not necessarily have a Rust toolchain.
  // Preserve an already-generated public WASM artifact when present; the
  // Docs Sync workflow builds and refreshes it on main.
  console.log('Synced playground shell; WASM artifact was not rebuilt in this environment.');
}

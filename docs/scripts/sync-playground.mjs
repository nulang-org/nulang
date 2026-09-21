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
  console.log('Synced browser playground, including freshly built WASM compiler.');
} catch {
  try {
    await access(targetWasm);
    console.log('Synced playground shell; preserving the generated public WASM compiler.');
  } catch {
    if (process.env.CF_PAGES === '1' && process.env.CF_PAGES_BRANCH === 'main') {
      throw new Error(
        'Refusing the production Cloudflare Pages build without docs/public/playground/nulang_playground.wasm. ' +
        'Docs Sync must generate and commit the compiler artifact first.'
      );
    }
    console.log(
      'Synced playground shell without WASM. Build playground/web first for a runnable local playground.'
    );
  }
}

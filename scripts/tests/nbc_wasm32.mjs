import { readFile } from 'node:fs/promises';
import { WASI } from 'node:wasi';

const path = process.argv[2];
if (!path) throw new Error('usage: node scripts/tests/nbc_wasm32.mjs <example.wasm>');
const wasi = new WASI({ version: 'preview1', args: [], env: {}, returnOnExit: true });
const { instance } = await WebAssembly.instantiate(await readFile(path), wasi.getImportObject());
process.exitCode = wasi.start(instance);

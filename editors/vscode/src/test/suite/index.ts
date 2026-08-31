import * as path from 'path';
import Mocha from 'mocha';
import { glob } from 'glob';

export async function run(): Promise<void> {
  const mocha = new Mocha({ ui: 'tdd', color: true, timeout: 90_000 });
  const testsRoot = path.resolve(__dirname, '.');
  const files = await glob('**/*.test.js', { cwd: testsRoot });
  for (const f of files) {
    mocha.addFile(path.resolve(testsRoot, f));
  }
  // Executor form, not Promise.withResolvers: the extension host runs in
  // Electron 27 (Node 18), which predates Promise.withResolvers.
  await new Promise<void>((resolve, reject) => {
    mocha.run((failures) => (failures > 0 ? reject(new Error(`${failures} tests failed`)) : resolve()));
  });
}

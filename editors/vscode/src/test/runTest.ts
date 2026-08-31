import * as path from 'path';
import { runTests } from '@vscode/test-electron';

async function main(): Promise<void> {
  try {
    const extensionDevelopmentPath = path.resolve(__dirname, '../..');
    const extensionTestsPath = path.resolve(__dirname, './suite/index');
    await runTests({
      extensionDevelopmentPath,
      extensionTestsPath,
      // Pinned stable: latest VS Code (1.134+) crashes its renderer under
      // xvfb on this machine; 1.96 is a known-stable headless-CI target.
      version: '1.96.0',
      // Headless-CI stability flags: no GPU, no Chromium sandbox (xvfb/root
      // environments), and no /dev/shm reliance (small containers).
      launchArgs: ['--disable-gpu', '--no-sandbox', '--disable-dev-shm-usage'],
    });
  } catch (err) {
    console.error('Failed to run extension tests:', err);
    process.exit(1);
  }
}

void main();

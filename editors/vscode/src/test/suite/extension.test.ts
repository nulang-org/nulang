import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';
import { createNulangTask, createStandaloneNulangTerminalOptions, selectNulangBinary } from '../../extension';

const GOOD_SOURCE = `fn add(a: Int, b: Int) -> Int {
  a + b
}
`;
const BAD_SOURCE = `fn broken( {
`;

function tmpNula(name: string, source: string): vscode.Uri {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'nulang-test-'));
  const file = path.join(dir, name);
  fs.writeFileSync(file, source);
  return vscode.Uri.file(file);
}

/**
 * Poll until `predicate(probe())` holds or the deadline passes. Polling is
 * required here: the language server is an external process whose readiness
 * is not exposed as an event, so there is no signal to await — this is a
 * deliberate real-clock integration wait, not a fixed sleep.
 */
async function waitFor<T>(
  probe: () => T | Promise<T>,
  predicate: (v: T) => boolean,
  timeoutMs: number
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let value = await probe();
  while (!predicate(value) && Date.now() < deadline) {
    // Executor form, not Promise.withResolvers: the extension host runs in
    // Electron 27 (Node 18), which predates Promise.withResolvers.
    await new Promise((resolve) => setTimeout(resolve, 500));
    value = await probe();
  }
  return value;
}

suite('Nulang extension', () => {
  test('nulang language id is registered', async () => {
    const languages = await vscode.languages.getLanguages();
    assert.ok(languages.includes('nulang'), 'nulang language id not registered');
  });

  test('LSP publishes diagnostics on open', async function () {
    this.timeout(90_000);
    const goodUri = tmpNula('good.nula', GOOD_SOURCE);
    const badUri = tmpNula('bad.nula', BAD_SOURCE);
    await vscode.workspace.openTextDocument(goodUri);
    await vscode.workspace.openTextDocument(badUri);

    const badDiags = await waitFor(
      () => vscode.languages.getDiagnostics(badUri),
      (d) => d.length > 0,
      60_000
    );
    assert.ok(badDiags.length > 0, 'broken document should have diagnostics');
    const goodDiags = vscode.languages.getDiagnostics(goodUri);
    assert.strictEqual(goodDiags.length, 0, `valid document should have no diagnostics: ${JSON.stringify(goodDiags)}`);
  });

  test('hover on a function declaration returns its signature', async function () {
    this.timeout(90_000);
    const uri = tmpNula('hover.nula', `${GOOD_SOURCE}\nlet x = add(1, 2)\n`);
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    const hovers = await waitFor(
      async () =>
        vscode.commands.executeCommand<vscode.Hover[]>('vscode.executeHoverProvider', uri, new vscode.Position(0, 3)),
      (h) => h.length > 0,
      60_000
    );
    assert.ok(hovers.length > 0, 'hover provider returned nothing');
    const contents = hovers
      .flatMap((h) => h.contents)
      .map((c) => (typeof c === 'string' ? c : c.value))
      .join('\n');
    assert.ok(contents.includes('add'), `hover should mention add: ${contents}`);
  });

  test('binary selection respects folder, workspace, global, then environment precedence', () => {
    assert.strictEqual(selectNulangBinary('/folder/tool', '/workspace/tool', '/global/tool', '/env/tool'), '/folder/tool');
    assert.strictEqual(selectNulangBinary(undefined, ' /workspace/tool ', '/global/tool', '/env/tool'), '/workspace/tool');
    assert.strictEqual(selectNulangBinary(undefined, undefined, '/global/tool', '/env/tool'), '/global/tool');
    assert.strictEqual(selectNulangBinary(undefined, undefined, undefined, ' /env/tool '), '/env/tool');
    assert.strictEqual(selectNulangBinary(undefined, undefined, undefined, undefined), 'nulang');
  });

  test('Run task passes shell-metacharacter paths literally and uses package root', () => {
    const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'nulang-task-'));
    try {
      const sourceDir = path.join(workspace, 'src');
      fs.mkdirSync(sourceDir);
      fs.writeFileSync(path.join(workspace, 'Nulang.toml'), '[package]\nname="task-smoke"\nversion="0.1.0"\n');
      const file = path.join(sourceDir, "it's a $literal; space.nula");
      fs.writeFileSync(file, GOOD_SOURCE);
      const binary = path.join(workspace, 'tools with spaces', 'nulang');
      const task = createNulangTask('Run', [], file, binary);
      assert.ok(task.execution instanceof vscode.ProcessExecution, 'file execution must bypass a shell');
      assert.strictEqual(task.execution.process, binary);
      assert.deepStrictEqual(task.execution.args, [file]);
      assert.strictEqual(task.execution.options?.cwd, workspace);
      assert.strictEqual(task.definition.command, 'Run');
      assert.notStrictEqual(task.scope, vscode.TaskScope.Global, 'VS Code does not support global tasks');
    } finally {
      fs.rmSync(workspace, { recursive: true, force: true });
    }
  });

  test('single-file mode preserves literal process args without task support', () => {
    const filename = "/tmp/it's $literal; project.nula";
    const options = createStandaloneNulangTerminalOptions(
      'Run', [], filename, '/toolchain with spaces/nulang'
    );
    assert.strictEqual(options.shellPath, '/toolchain with spaces/nulang');
    assert.deepStrictEqual(options.shellArgs, [filename]);
    assert.strictEqual(options.cwd, undefined, 'empty workspaces cannot set arbitrary terminal cwd');
  });

  test('Type Check task uses standalone file directory without a manifest', () => {
    const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'nulang-task-'));
    try {
      const file = path.join(workspace, 'standalone file.nula');
      fs.writeFileSync(file, BAD_SOURCE);
      const task = createNulangTask('Type Check', ['--check'], file, 'nulang');
      assert.ok(task.execution instanceof vscode.ProcessExecution);
      assert.deepStrictEqual(task.execution.args, ['--check', file]);
      assert.strictEqual(task.execution.options?.cwd, workspace);
    } finally {
      fs.rmSync(workspace, { recursive: true, force: true });
    }
  });
});

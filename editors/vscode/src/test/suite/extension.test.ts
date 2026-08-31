import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';

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
});

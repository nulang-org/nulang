import * as vscode from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions } from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

/**
 * Resolve the nulang binary: explicit `nulang.path` setting first, then the
 * `NULANG_PATH` environment variable, then `nulang` on PATH.
 */
function nulangBinary(): string {
  const config = vscode.workspace.getConfiguration('nulang');
  const inspected = config.inspect<string>('path');
  const explicit = inspected?.globalValue ?? inspected?.workspaceValue;
  if (explicit && explicit.trim() !== '') {
    return explicit.trim();
  }
  const envPath = process.env.NULANG_PATH;
  if (envPath && envPath.trim() !== '') {
    return envPath.trim();
  }
  return 'nulang';
}

/** The active editor's file, but only when it is a Nulang document. */
function activeNulaFile(): string | undefined {
  const editor = vscode.window.activeTextEditor;
  if (editor && editor.document.languageId === 'nulang') {
    return editor.document.fileName;
  }
  return undefined;
}

/** Run the nulang CLI on the active .nula file in a terminal. */
function runNulang(args: string[]): void {
  const file = activeNulaFile();
  if (!file) {
    vscode.window.showWarningMessage('Open a .nula file first.');
    return;
  }
  const terminal = vscode.window.createTerminal({ name: 'Nulang' });
  terminal.sendText(`"${nulangBinary()}" ${args.join(' ')} "${file}"`);
  terminal.show();
}

export function activate(context: vscode.ExtensionContext): void {
  // No `transport` field: with TransportKind.stdio the client appends
  // `--stdio` to the args (a node-server convention), which nulang rejects.
  // Undefined transport still uses stdio pipes, without the extra flag.
  const serverOptions: ServerOptions = {
    command: nulangBinary(),
    args: ['--lsp'],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ language: 'nulang', scheme: 'file' }],
    outputChannelName: 'Nulang Language Server',
    diagnosticPullOptions: { onChange: true, onSave: true },
  };

  client = new LanguageClient('nulang', 'Nulang Language Server', serverOptions, clientOptions);
  client.start().catch((err) => {
    console.error(`[nulang] language server failed to start (binary: ${nulangBinary()}):`, err);
  });

  context.subscriptions.push(
    vscode.commands.registerCommand('nulang.restartServer', async () => {
      await client?.restart();
    }),
    vscode.commands.registerCommand('nulang.compile', () => runNulang(['--emit-nbc'])),
    vscode.commands.registerCommand('nulang.run', () => runNulang([])),
    vscode.commands.registerCommand('nulang.typeCheck', () => runNulang(['--check']))
  );
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

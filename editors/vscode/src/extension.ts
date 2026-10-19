import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions } from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

/** Choose an explicitly configured binary before consulting the environment. */
export function selectNulangBinary(
  folderValue?: string, workspaceValue?: string, globalValue?: string, envValue?: string
): string {
  for (const value of [folderValue, workspaceValue, globalValue, envValue]) {
    if (value?.trim()) {
      return value.trim();
    }
  }
  return 'nulang';
}

/** VS Code settings are scoped to the active source file. */
function nulangBinary(uri?: vscode.Uri): string {
  const config = vscode.workspace.getConfiguration('nulang', uri);
  const inspected = config.inspect<string>('path');
  return selectNulangBinary(
    inspected?.workspaceFolderValue,
    inspected?.workspaceValue,
    inspected?.globalValue,
    process.env.NULANG_PATH
  );
}

/** Prefer the nearest Nulang.toml; standalone files run from their own folder. */
function packageDirectory(file: string): string {
  const sourceDirectory = path.dirname(path.resolve(file));
  let directory = sourceDirectory;
  for (;;) {
    if (fs.existsSync(path.join(directory, 'Nulang.toml'))) {
      return directory;
    }
    const parent = path.dirname(directory);
    if (parent === directory) {
      return sourceDirectory;
    }
    directory = parent;
  }
}

/**
 * Run as a process, not a shell command. File names and configured executable
 * paths can contain whitespace or shell metacharacters without interpretation.
 */
export function createNulangTask(
  command: string, args: string[], file: string, binary: string
): vscode.Task {
  const execution = new vscode.ProcessExecution(binary, [...args, file], {
    cwd: packageDirectory(file),
  });
  const task = new vscode.Task(
    { type: 'nulang', command },
    vscode.TaskScope.Global,
    `Nulang: ${command}`,
    'nulang',
    execution
  );
  task.presentationOptions = {
    reveal: vscode.TaskRevealKind.Always,
    panel: vscode.TaskPanelKind.Dedicated,
  };
  return task;
}

/** Run the active .nula file in a managed task with visible terminal output. */
async function runNulang(command: string, args: string[]): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== 'nulang' || editor.document.uri.scheme !== 'file') {
    vscode.window.showWarningMessage('Open a local .nula file first.');
    return;
  }
  const file = editor.document.fileName;
  const task = createNulangTask(command, args, file, nulangBinary(editor.document.uri));
  try {
    await vscode.tasks.executeTask(task);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    vscode.window.showErrorMessage(`Could not start Nulang ${command}: ${message}`);
  }
}

export function activate(context: vscode.ExtensionContext): void {
  // No `transport` field: with TransportKind.stdio the client appends
  // `--stdio` to the args (a node-server convention), which nulang rejects.
  // Undefined transport still uses stdio pipes, without the extra flag.
  const uri = vscode.window.activeTextEditor?.document.uri;
  const serverOptions: ServerOptions = {
    command: nulangBinary(uri),
    args: ['--lsp'],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ language: 'nulang', scheme: 'file' }],
    outputChannelName: 'Nulang Language Server',
    diagnosticPullOptions: { onChange: true, onSave: true },
  };

  client = new LanguageClient('nulang', 'Nulang Language Server', serverOptions, clientOptions);
  client.start().catch((err) => {
    console.error(`[nulang] language server failed to start (binary: ${serverOptions.command}):`, err);
  });

  context.subscriptions.push(
    vscode.commands.registerCommand('nulang.restartServer', async () => {
      await client?.restart();
    }),
    vscode.commands.registerCommand('nulang.compile', () => runNulang('Compile', ['--emit-nbc'])),
    vscode.commands.registerCommand('nulang.run', () => runNulang('Run', [])),
    vscode.commands.registerCommand('nulang.typeCheck', () => runNulang('Type Check', ['--check']))
  );
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

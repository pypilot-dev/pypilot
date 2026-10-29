# LSP protocol

`pypilot lsp` is a standard language server over stdio. Any LSP client can run it; Zed and VS Code do. This page describes what it offers, from the server code (`helper/src/lsp/mod.rs`, `lsp/pixi.rs`, `lsp/diagnostics.rs`).

::: info See also
The authoritative, versioned protocol reference lives next to the code: [`helper/src/lsp/PROTOCOL.md`](https://github.com/pypilot-dev/pypilot/blob/main/helper/src/lsp/PROTOCOL.md). If the two disagree, PROTOCOL.md wins. <!-- verify after merge: PROTOCOL.md exists and this page matches it -->
:::

## Initialization

- **Project root**: the first of `workspaceFolders`, else `rootUri`. If the folder's only Python project is in one subfolder, that subfolder is used. One server handles one root.
- **`initializationOptions`**: editor settings (see [Configuration](#configuration)).
- **`clientInfo.name`**: `Zed` (or `Zed …`) enables the Zed-only offer to point pyright at the project environment.
- **Server info**: `{ "name": "PyPilot", "version": "<engine version>" }`.

### Server capabilities

```json
{
  "textDocumentSync": 1,
  "codeActionProvider": true,
  "executeCommandProvider": {
    "commands": [
      "pypilot.fixEverything", "pypilot.showDetails", "pypilot.ignore",
      "pypilot.neverForProject", "pypilot.installPackage",
      "pypilot.recreateWithPython", "pypilot.installUvGlobally",
      "pypilot.installPixi", "pypilot.migrateToPixi"
    ]
  }
}
```

Text sync is **full** (`1`): send the whole document on each change.

## Documents

| Notification | Behaviour |
|---|---|
| `textDocument/didOpen` | Analyze now |
| `textDocument/didChange` | Analyze one second after the last edit; newer edits supersede pending runs |
| `textDocument/didSave` | Analyze now |
| `textDocument/didClose` | Forget the buffer and clear its diagnostics |

Only `file:` URIs ending in `.py` are analyzed, plus Jupyter cells under the `vscode-notebook-cell:` scheme with `languageId: "python"` (IPython magics and `!` lines are masked).

## Diagnostics

`textDocument/publishDiagnostics` with `source: "pypilot"` on import lines. Each diagnostic carries its fix in `data`, so code actions are rebuilt without re-analysis:

```json
{
  "package": "opencv-python",
  "action": { "kind": "recreate", "python": "3.12", "reinstall_count": 4 },
  "pixi": false
}
```

`action.kind` is `install`, `create` (with `python`), `recreate` (with `python`, `reinstall_count`) or `none`. `pixi` is omitted when false. Messages and severities are listed on [What PyPilot reports](./checks#import-diagnostics-editor).

## Code actions

`textDocument/codeAction` returns quick fixes for the diagnostics in the request's context: one per fixable diagnostic, one that installs every selected package in a single command, and one that covers the whole file (or the whole notebook, across its cells). Each action runs one of the commands below.

## Commands

`workspace/executeCommand`. Commands that change the environment answer `{ "ok": bool }`, so a client can tell a failed setup from a successful one; the others answer `null`.

| Command | Arguments | Result | Does |
|---|---|---|---|
| `pypilot.fixEverything` | — | `{ ok }` | Same as `pypilot setup` (asks how to proceed first when uv is missing in `auto` mode) |
| `pypilot.showDetails` | `[package?]` | `null` | Writes the report to `pypilot-report.md` in the temp directory and opens it with `window/showDocument`; with a package, adds a `pypilot check` section. Falls back to `window/showMessage` |
| `pypilot.ignore` | — | `null` | Nothing (dismisses) |
| `pypilot.neverForProject` | — | `null` | Writes `[state] never = true` to `.zed/pypilot.toml` |
| `pypilot.installPackage` | `[package, …]` | `{ ok }` | Installs one or more packages in one step |
| `pypilot.recreateWithPython` | `[python, package, …]` | `{ ok }` | Rebuilds the environment on `python` once, then installs every package |
| `pypilot.installUvGlobally` (alias `pypilot.installUv`) | — | `{ ok }` | Installs uv |
| `pypilot.installPixi` <Badge type="tip" text="next" /> | — | `{ ok }` | Runs pixi's official installer; a failure is also shown with `window/showMessage` |
| `pypilot.migrateToPixi` <Badge type="tip" text="next" /> | `[{ write?: bool, install?: bool }]` | see below | Previews or writes `pixi.toml` |

`pypilot.migrateToPixi`:

- without `write`: `{ "preview": "<diff>" }`. No `preview` means there is nothing to do, and the reason was already shown to the user.
- with `write`: `{ "written": ["<absolute path>"], "ok": bool }`; with `install` too, `pixi install` runs after writing.

## Messages to the client

| Method | When |
|---|---|
| `window/showMessageRequest` | The workspace notification (buttons **Fix environment**, **Show details**, **Ignore**, **Never for this project**, sometimes **Initialize pixi project**), and confirmations such as installing uv or pixi, choosing a uv mode, or the pyright pin |
| `window/showMessage` | Progress and results of actions (`PyPilot: installing numpy…`, failures) |
| `window/showDocument` | The doctor report |
| `window/logMessage` | Unknown commands and other diagnostics for the log |

The button texts are what the user clicks; the server acts on them itself, so a client only needs to display the request.

## Configuration

Settings arrive in `initializationOptions` and `workspace/didChangeConfiguration` (`params.settings`), either flat or under a `pypilot` key, camelCase:

```json
{ "pypilot": { "packageManager": "uv", "notifications": "all", "autoCheckOnOpen": true,
               "dataRefreshDays": 7, "pixiEnvironment": "default" } }
```

They sit between the global config file and the project file; values equal to the default count as unset; bad values are ignored with a log line. A configuration change re-checks every open buffer. Details: [Configuration](./configuration#editor-settings-over-lsp).

## Coming in the next release

<Badge type="tip" text="next" /> `diagnosticsScope`: `openFiles` (default) or `workspace`, in `initializationOptions` and `settings.pypilot.diagnosticsScope`; with `workspace` the server scans every Python file in the background and publishes diagnostics for files that are not open.

## Using it from another editor

See [Other editors](./editors) for Neovim, Helix and Emacs configurations.

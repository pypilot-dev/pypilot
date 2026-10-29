# LSP protocol

`pypilot lsp` is a standard language server: JSON-RPC over stdio, LSP 3.17. Any LSP client can run it; Zed and VS Code do. This page is a guided summary.

::: info The reference
<Badge type="tip" text="1.3" /> The authoritative protocol reference lives next to the code: [`helper/src/lsp/PROTOCOL.md`](https://github.com/pypilot-dev/pypilot/blob/main/helper/src/lsp/PROTOCOL.md). It is kept in step with `helper/src/lsp/` and `helper/src/settings.rs`. If this page and PROTOCOL.md disagree, PROTOCOL.md wins.
:::

## Launching

```bash
pypilot lsp
```

No arguments and no environment variables. Use the `pypilot` that `pypilot-cli`, `cargo install` or a release archive put on your `PATH`, or the one the editor extensions download.

- **Project root**: the first of `workspaceFolders`, else `rootUri`. If that folder has no Python of its own but exactly one subfolder (up to two levels down) has a `pyproject.toml`, `requirements.txt` or `pixi.toml`, that subfolder is the project. One server handles one root; without one it answers requests but does nothing.
- **`initializationOptions`**: editor settings (see [Configuration](#configuration)).
- **`clientInfo.name`**: `Zed` or `Zed <channel>` turns on the Zed-only offer to point pyright at the project environment.
- **Server info**: `{ "name": "PyPilot", "version": "<engine version>" }`.

## Capabilities

| Capability | Value |
|---|---|
| `textDocumentSync` | `1` (Full): send the whole document on each change |
| `codeActionProvider` | `true` |
| `executeCommandProvider.commands` | the ids in [Commands](#commands) |

Handled notifications: `initialized`, `textDocument/didOpen`, `didChange`, `didSave`, `didClose`, `workspace/didChangeConfiguration` and <Badge type="tip" text="1.3" /> `workspace/didChangeWatchedFiles`.

Requests the server sends: `window/showMessageRequest`, `window/showDocument`, and <Badge type="tip" text="1.3" /> `client/registerCapability` (only to watch `**/*.py` and `**/*.ipynb`, only with `diagnosticsScope = "workspace"`, and only when the client declares `workspace.didChangeWatchedFiles.dynamicRegistration`). Notifications it sends: `textDocument/publishDiagnostics`, `window/showMessage`, `window/logMessage`. It never sends `workspace/configuration`: settings must be pushed.

## Documents

| Notification | Behaviour |
|---|---|
| `textDocument/didOpen` | Analyze now |
| `textDocument/didChange` | Analyze one second after the last edit; newer edits supersede pending runs |
| `textDocument/didSave` | Analyze now |
| `textDocument/didClose` | Forget the buffer and clear its diagnostics (with workspace diagnostics, re-check the file from disk) |

Only `file:` URIs ending in `.py` are analyzed, plus VS Code notebook cells (scheme `vscode-notebook-cell`) whose `languageId` is `python`. IPython magics, shell escapes and `?` help lines are masked before analysis; diagnostics are published per cell URI.

## Diagnostics

`textDocument/publishDiagnostics`, `source: "pypilot"`, no `code`, with the `range` on the module path as written in the import:

| Problem | Severity | Message starts | Fix in `data` |
|---|---|---|---|
| not installed | Warning | `` `pkg` is not installed. `` | install |
| no environment yet | Warning | `` `pkg` is not installed, and this project has no virtual environment yet. `` | create on Python X.Y |
| no wheels for this Python | Error | `` `pkg` has no wheels for Python 3.x. It supports … `` | rebuild on Python X.Y |
| attribute missing in the installed release | Error | ``The installed `pkg` 1.2 has no `attr`.`` | none |
| CUDA extension that would break torch | Error or Warning | explanation | none |
| no files for this platform | Error | `` `pkg`: … `` | none |
| unknown to PyPI | Warning | ``PyPI has no project named `pkg`.`` | none, deliberately (typosquatting) |

When an import resolves to a different distribution, the message says `` `opencv-python` (provides `cv2`) ``. A broken environment (its Python is gone) publishes no import diagnostics; the workspace notification reports it instead.

<Badge type="tip" text="1.3" /> **Offline.** "PyPI has no project named …" needs a real 404 from PyPI. When PyPI cannot be reached (offline, a proxy, an outage), imports whose metadata is not cached get no diagnostic at all, rather than a false "check the spelling". Engine 1.2.2 reported every failed lookup as unknown to PyPI.

Each diagnostic carries its fix in `data`, so code actions never re-analyze:

```json
{ "package": "opencv-python", "action": { "kind": "install" }, "pixi": true }
```

`action.kind` is `install`, `create` (with `python`), `recreate` (with `python`, `reinstall_count`) or `none`. `pixi` is present only when `true`. The user-facing list is on [What PyPilot reports](./checks#import-diagnostics-editor).

### Workspace diagnostics <Badge type="tip" text="1.3" /> {#workspace-diagnostics}

With `diagnosticsScope = "workspace"` the server also scans the project's `.py` files and notebooks on disk, off the main task, and publishes the same diagnostics for every file that is **not open**:

- **Skipped**: hidden directories (`.venv`, `.pixi`, `.git`, …), `venv`, `env`, `node_modules`, `__pycache__`, `target`, `build`, `dist`, `site-packages`, `conda-meta`, any directory with a `pyvenv.cfg`, and what the project's `.gitignore` files exclude (globs, `**`, `!`, trailing `/`, anchoring; not `.git/info/exclude` or the global excludes file).
- **Bounded**: 5,000 files, 200,000 directory entries, `.py` files up to 1 MiB, notebooks up to 16 MiB. Past a cap a `window/logMessage` says so.
- **Notebooks on disk** are published on their `file:` URI, with the `range` at 0:0 and the message suffixed `(cell N, line L)`.
- **Debounced** (750 ms) and restarted by newer requests; files with nothing to report send no message.
- **Open documents win**: an open file (or any open cell of a notebook) is never published from disk; opening it clears the scan's diagnostics; closing it re-checks it from disk.
- **Re-checked**: files reported by `workspace/didChangeWatchedFiles` (500 ms debounce; events in skipped directories are ignored), and everything after an install, a rebuild, **Fix environment**, a CLI rescan request or a configuration change.
- Switching back to `openFiles` publishes empty diagnostics for every file the scan reported.

## Code actions

`textDocument/codeAction` answers quick fixes (`kind: "quickfix"`), each a `CodeAction` with a `command` and no `edit`, built from the `data` of the PyPilot diagnostics in `context.diagnostics`:

1. one combined action per fix kind when two or more packages are selected (`isPreferred: true`);
2. one action per package (preferred when there is no combined one);
3. one action for the whole file (or every open cell of the notebook) when it has two or more installable packages the selection does not already cover.

Titles look like ``Install `numpy` ``, `Install 3 packages: a, b, c`, ``Create environment on Python 3.12 and install `x` ``, ``Rebuild environment on Python 3.12 and install `x` (2 existing packages will be reinstalled)`` and `Install all 3 missing packages in this file`; in a pixi project ``Add `x` with pixi`` and ``Move the pixi environment to Python 3.12 and add `x` ``.

## Commands

`workspace/executeCommand`. Every command needs a workspace root (else the result is `null`). Commands that change the environment answer `{ "ok": bool }`, so a client can tell a failed setup from a successful one; progress and errors also go out as `window/showMessage`.

| Command | Arguments | Result | Does |
|---|---|---|---|
| `pypilot.fixEverything` | none | `{ ok }` | Same as `pypilot setup` (asks how to proceed first when uv is missing in `auto` mode) |
| `pypilot.showDetails` | `[package?]` | `null` | Writes the report to `<temp>/pypilot-report.md` and opens it with `window/showDocument`; with a package (or a requirement such as `numpy>=2`), adds a `pypilot check` section. Falls back to `window/showMessage` |
| `pypilot.ignore` | none | `null` | Nothing (dismisses) |
| `pypilot.neverForProject` | none | `null` | Writes `[state] never = true` to `.zed/pypilot.toml` |
| `pypilot.installPackage` | `[package, …]` | `{ ok }` | Installs one or more packages in one step; names are validated |
| `pypilot.recreateWithPython` | `[python, package, …]` | `{ ok }` | Builds (or rebuilds) the environment on `python`, then installs every package |
| `pypilot.installUvGlobally` (alias `pypilot.installUv`) | none | `{ ok }` | Installs uv; `true` if already present |
| `pypilot.installPixi` <Badge type="tip" text="1.3" /> | none | `{ ok }` | Runs pixi's official installer |
| `pypilot.migrateToPixi` <Badge type="tip" text="1.3" /> | `[{ write?: bool, install?: bool }]` | see below | Previews or writes `pixi.toml` |

`pypilot.migrateToPixi`:

- without `write`: `{ "preview": "<diff>" }`. No `preview` means there is nothing to do, and the reason was already shown to the user.
- with `write`: `{ "written": ["<absolute path>"], "ok": bool }`; with `install` too, `pixi install` runs after writing.

A malformed call (missing or non-string package arguments) and an unknown command answer `null`; the unknown command is also logged.

## Messages to the user

`window/showMessageRequest` buttons are matched by their exact title, and the server acts on the answer itself, so a client only has to display the request:

| When | Buttons |
|---|---|
| The workspace check after `initialized` (if `autoCheckOnOpen`, not dismissed, `notifications` not `off`), or a CLI rescan request, when something needs attention | `Fix environment`, `Initialize pixi project` (only for a pixi-worthy finding), `Show details`, `Ignore`, `Never for this project` |
| "Fix environment" with `packageManager = auto` and no uv on `PATH` (not in pixi projects) | `Install uv`, `No thanks`; after `Install uv`: `uv project (pyproject.toml + lockfile)`, `pip-compatible (requirements.txt, faster via uv)` |
| A pip/uv build failed on a system library conda-forge ships | `Initialize pixi project`, `Ignore` |
| "Initialize pixi project" | `Write and install`, `Write pixi.toml`, `Cancel` (after a dry run; see [Migrating to pixi](/pixi/migrate#from-the-editor)) |
| pixi is needed but missing | `Install pixi`, `Not now` |
| Zed only, after PyPilot built the environment and pyright is not configured | `Point pyright at the environment`, `Not now` |

An explicit rescan with nothing wrong shows an Info `showMessage` ("PyPilot: everything looks good. …"). Every user-facing text starts with `PyPilot:`.

## Configuration

Settings arrive in `initializationOptions` (flat, or nested under `pypilot`) and in `workspace/didChangeConfiguration` as `{ "settings": { "pypilot": { … } } }` (flat accepted too). Keys are camelCase:

```json
{ "pypilot": { "packageManager": "uv", "notifications": "all", "autoCheckOnOpen": true,
               "dataRefreshDays": 7, "pixiEnvironment": "default", "diagnosticsScope": "workspace" } }
```

| Key | Values | Default |
|---|---|---|
| `packageManager` | `auto`, `uv`, `uv-pip`, `pip`, `pixi` | `auto` |
| `notifications` | `all`, `problems-only`, `off` | `problems-only` |
| `autoCheckOnOpen` | bool | `true` |
| `dataRefreshDays` | integer | `7` |
| `pixiEnvironment` | pixi environment name (`[a-z0-9-]+`) | `default` |
| `diagnosticsScope` <Badge type="tip" text="1.3" /> | `openFiles`, `workspace` | `openFiles` |

They sit between the global config file and the project file; values equal to the default count as unset; bad values are ignored with a line on stderr. **Each message replaces the whole editor layer**, so send all keys the same way. A configuration change re-checks every open buffer and starts, re-runs or clears the workspace scan. Details: [Configuration](./configuration#editor-settings-over-lsp).

## CLI rescan requests

Zed extensions cannot add palette commands, so the Zed tasks run `pypilot setup` or `pypilot doctor` in a terminal. Those (and `fix`, `install`, `migrate-conda`, `migrate-to-pixi`) write a request file, `<cache dir>/rescan/<16 hex digits>.req`, named from a hash of the workspace path. The server checks its modification time once a second and, when it changes, re-runs the workspace check as an explicit request, re-checks open buffers and re-runs the workspace scan. Other editors get this by running the same CLI commands. `setup`, `fix` and `install` with `--dry-run` write no request.

## Using it from another editor

See [Other editors](./editors) for Neovim, Helix and Emacs.

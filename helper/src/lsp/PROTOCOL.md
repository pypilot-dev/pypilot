# PyPilot language server protocol surface

Everything an editor integration needs to talk to `pypilot lsp`: what the
server advertises, which settings it reads, every command it executes, the
messages it sends, and the diagnostics and code actions it publishes. Zed
(`extension/`) and VS Code (`pypilot-vscode`) use exactly this; Neovim, Helix
and Emacs can too (snippets at the end).

The source of truth is `helper/src/lsp/` (`mod.rs`, `diagnostics.rs`,
`pixi.rs`, `workspace_scan.rs`) and `helper/src/settings.rs`. Keep this file
in step with them.

## Launching

```
pypilot lsp
```

JSON-RPC over stdio, LSP 3.17, implemented with tower-lsp. No arguments, no
environment variables. The binary is `~/.cargo/bin/pypilot`, the one the
`pypilot-cli` pip package installs, or the one the Zed and VS Code extensions
download.

The workspace root is the first `workspaceFolders` entry, else `rootUri`. If
that folder has no Python of its own but exactly one subfolder has a
`pyproject.toml`, `requirements.txt` or `pixi.toml` (up to two levels down),
that subfolder is the project. Without a root the server answers requests
but does nothing.

`clientInfo.name` of `Zed` or `Zed <channel>` turns on the Zed-only pyright
pin offer (see [Messages](#messages-to-the-user)).

## Server capabilities

| Capability | Value |
| --- | --- |
| `textDocumentSync` | `1` (Full) |
| `codeActionProvider` | `true` |
| `executeCommandProvider.commands` | the ids in [Commands](#workspaceexecutecommand) |

Handled notifications: `initialized`, `textDocument/didOpen`, `didChange`,
`didSave`, `didClose`, `workspace/didChangeConfiguration`,
`workspace/didChangeWatchedFiles`. The server does not send
`workspace/configuration` requests; settings must be pushed.

Requests the server sends to the client: `window/showMessageRequest`,
`window/showDocument`, and `client/registerCapability` (only for
`workspace/didChangeWatchedFiles` with `**/*.py` and `**/*.ipynb`, only in
workspace diagnostics mode, and only when the client declares
`workspace.didChangeWatchedFiles.dynamicRegistration`). Notifications it
sends: `textDocument/publishDiagnostics`, `window/showMessage`,
`window/logMessage`.

## Settings

The same keys arrive in three places:

1. `initializationOptions` of `initialize`: a flat object
   (`{ "diagnosticsScope": "workspace" }`), or the same nested under `pypilot`.
2. `workspace/didChangeConfiguration`: `{ "settings": { "pypilot": { ... } } }`
   (flat also accepted).
3. TOML files, keys in snake_case: the global
   `<config dir>/pypilot/config.toml` (Linux `~/.config/pypilot/config.toml`,
   Windows `%APPDATA%\PyPilot\pypilot\config\config.toml`, macOS
   `~/Library/Application Support/dev.PyPilot.pypilot/config.toml`) and the
   project's `.zed/pypilot.toml`, else `pypilot.toml`.

Precedence, per key: **global file < editor < project file**. The project
file wins because it is shared with the team. An editor value equal to the
built-in default counts as unset (VS Code sends every setting, defaults
included, and those must not mask the global file). A value that does not
parse is ignored with a line on stderr, never an error.

Every `initializationOptions` or `didChangeConfiguration` **replaces** the
whole editor layer. A client that sends initialization options and then an
empty or unrelated `didChangeConfiguration` loses the first set, so send all
keys the same way.

| Editor key | TOML key | Values | Default | Effect |
| --- | --- | --- | --- | --- |
| `packageManager` | `package_manager` | `auto`, `uv`, `uv-pip`, `pip`, `pixi` | `auto` | how environments are built and packages installed; a pixi manifest always means pixi |
| `notifications` | `notifications` | `all`, `problems-only`, `off` | `problems-only` | toast verbosity; `off` also silences the import diagnostics |
| `autoCheckOnOpen` | `auto_check_on_open` | bool | `true` | run the onboarding check after `initialized` |
| `dataRefreshDays` | `data_refresh_days` | integer | `7` | how often the bundled data tables are re-downloaded; `0` = never |
| `pixiEnvironment` | `pixi_environment` | pixi env name (`[a-z0-9-]+`) | `default` | which pixi environment is the project's |
| `diagnosticsScope` | `diagnostics_scope` | `openFiles`, `workspace` (TOML also `open-files`) | `openFiles` | which files get import diagnostics, see [Workspace diagnostics](#workspace-diagnostics) |

Keys the server does not know (`autoSelectVenv` and other client-only
settings) are ignored. The project file also stores `[state] never = true`
("Never for this project"), written by the server.

A new configuration re-checks every open buffer (the pixi environment may
have changed) and starts, re-runs or clears the workspace scan.

## Diagnostics

Published with `textDocument/publishDiagnostics` for open Python buffers
(`file:` URIs ending in `.py`, and VS Code notebook cells, see below),
immediately on open and save and one second after the last edit. Each has:

- `source`: `"pypilot"`. No `code`.
- `range`: the module path as written in the import statement.
- `severity` and `message` by problem:

| Problem | Severity | Message starts | Automatable fix |
| --- | --- | --- | --- |
| not installed | Warning | `` `pkg` is not installed. `` | install |
| no environment yet | Warning | `` `pkg` is not installed, and this project has no virtual environment yet. `` | create on Python X.Y |
| no wheels for this Python | Error | `` `pkg` has no wheels for Python 3.x. It supports … `` | rebuild on Python X.Y |
| attribute missing in installed release | Error | ``The installed `pkg` 1.2 has no `attr`.`` | none |
| CUDA extension that would break torch | Error or Warning | explanation | none |
| no files for this platform | Error | `` `pkg`: … `` | none |
| unknown to PyPI | Warning | ``PyPI has no project named `pkg`.`` | none, deliberately (typosquatting) |

When an import resolves to a different distribution (`cv2` to
`opencv-python`) the message says `` `opencv-python` (provides `cv2`) ``.

- `data` carries the fix, so code actions never re-analyze:

  ```json
  { "package": "opencv-python",
    "action": { "kind": "install" },
    "pixi": true }
  ```

  `action.kind` is `install`, `create` (`python`), `recreate` (`python`,
  `reinstall_count`) or `none`. `pixi` is present only when `true`.

A broken environment (its Python is gone) publishes no import diagnostics;
the onboarding toast reports it instead. `didClose` clears the buffer's
diagnostics.

**Notebooks.** VS Code sends each cell as a document with scheme
`vscode-notebook-cell` (`vscode-notebook-cell:/c%3A/p/a.ipynb#W2s…`). Cells
with `languageId` other than `python` are skipped; IPython magics, shell
escapes and `?` help lines are masked before analysis. Diagnostics are
published per cell URI.

### Workspace diagnostics

With `diagnosticsScope = "workspace"` the server also scans the project's
`.py` files and notebooks on disk, off the main task, and publishes the same
diagnostics for every file that is **not open**:

- skipped: hidden directories (`.venv`, `.pixi`, `.git`, …), `venv`, `env`,
  `node_modules`, `__pycache__`, `target`, `build`, `dist`, `site-packages`,
  `conda-meta`, any directory with a `pyvenv.cfg`, and whatever the
  project's `.gitignore` files exclude (globs, `**`, `!`, trailing `/`,
  anchoring; not `.git/info/exclude` or the global excludes file);
- bounded: 5,000 files, 200,000 directory entries, `.py` files up to 1 MiB
  and notebooks up to 16 MiB; past a cap a `window/logMessage` says so;
- a notebook on disk is published on its `file:` URI, with `range` at 0:0
  and the message suffixed `(cell N, line L)`;
- debounced (750 ms) and restarted by newer requests; files with nothing to
  report send no message;
- open documents win: an open file (or any open cell of a notebook) is never
  published from disk, and opening it first clears the scan's diagnostics;
  closing it re-checks the file from disk;
- re-checked: changed files from `workspace/didChangeWatchedFiles` (500 ms
  debounce; events inside skipped directories are ignored), everything
  after an install, rebuild, "Fix environment", a CLI rescan request and a
  configuration change;
- switching back to `openFiles` publishes empty diagnostics for every file
  the scan reported.

## Code actions

`textDocument/codeAction` answers quick fixes (`kind: "quickfix"`), each a
`CodeAction` with a `command` (no `edit`), built from the `data` of the
PyPilot diagnostics in `context.diagnostics` (others are ignored):

1. one combined action per fix kind when two or more packages are selected,
   preferred (`isPreferred: true`);
2. one action per package (preferred when there is no combined one);
3. one action covering the whole file (or every open cell of the notebook)
   when it has two or more installable packages the selection does not
   already cover.

Titles: ``Install `numpy` ``, `Install 3 packages: a, b, c`,
``Create environment on Python 3.12 and install `x` ``,
``Rebuild environment on Python 3.12 and install `x` (2 existing packages will be reinstalled)``,
`Install all 3 missing packages in this file`; in a pixi project
``Add `x` with pixi`` and ``Move the pixi environment to Python 3.12 and add `x` ``.
Platform caveats are appended in parentheses (for example tensorflow being
CPU-only on native Windows).

## `workspace/executeCommand`

All commands need a workspace root; without one the result is `null`.
Commands that change the environment answer `{ "ok": bool }` so a client can
tell a failure from a success (Zed ignores results). Progress and errors are
also shown with `window/showMessage`.

| Command | Arguments | Result | Does |
| --- | --- | --- | --- |
| `pypilot.fixEverything` | none | `{ "ok": bool }` | set up the environment (`pypilot setup`); may first ask to install uv, see below |
| `pypilot.showDetails` | `[package?]` | `null` | write the report to `<temp>/pypilot-report.md` and `window/showDocument` it (falls back to `showMessage`); with a package (or requirement like `numpy>=2`), adds a `pypilot check` section |
| `pypilot.ignore` | none | `null` | nothing (the toast's Ignore) |
| `pypilot.neverForProject` | none | `null` | write `[state] never = true` to `.zed/pypilot.toml`; the onboarding toast stops |
| `pypilot.installPackage` | `[package, ...]` (strings, at least one) | `{ "ok": bool }` | install into the project environment; names are validated |
| `pypilot.recreateWithPython` | `[python, package, ...]` | `{ "ok": bool }` | build (or rebuild) the environment on that Python, then install the packages |
| `pypilot.installUvGlobally` (alias `pypilot.installUv`) | none | `{ "ok": bool }` | install uv with its official installer; `true` if already present |
| `pypilot.installPixi` | none | `{ "ok": bool }` | install pixi with its official installer |
| `pypilot.migrateToPixi` | `[{ "write"?: bool, "install"?: bool }]` | without `write`: `{ "preview": "<diff>" }` (no `preview` when there is nothing to do); with `write`: `{ "written": ["<path>"], "ok": bool }` | write a `pixi.toml` from the project's dependencies, optionally `pixi install` |

A malformed call (missing or non-string package arguments) answers `null`.
Unknown commands answer `null` and log a warning.

## Messages to the user

`window/showMessageRequest` buttons are matched by exact title:

| When | Type | Buttons |
| --- | --- | --- |
| onboarding check after `initialized` (if `autoCheckOnOpen`, not dismissed, `notifications` not `off`) or a CLI rescan request, when something needs attention | Error / Warning / Info | `Fix environment`, `Initialize pixi project` (only for a pixi-worthy finding), `Show details`, `Ignore`, `Never for this project` |
| "Fix environment" with `packageManager = auto` and no uv on PATH (not in pixi projects) | Info | `Install uv`, `No thanks`; after `Install uv`: `uv project (pyproject.toml + lockfile)`, `pip-compatible (requirements.txt, faster via uv)` |
| a pip/uv build failed on a system library conda-forge ships | Info | `Initialize pixi project`, `Ignore` |
| "Initialize pixi project" | Info | `Write and install`, `Write pixi.toml`, `Cancel` |
| pixi is needed but missing | Warning | `Install pixi`, `Not now` |
| Zed only, after PyPilot built the environment and pyright is not configured | Info | `Point pyright at the environment`, `Not now` (writes `[tool.pyright]` in `pyproject.toml` or a `pyrightconfig.json`) |

An explicit rescan with nothing wrong shows an Info `showMessage`
("PyPilot: everything looks good. …"). Every user-facing text starts with
`PyPilot:`.

## CLI rescan requests (Zed tasks)

Zed extensions cannot add palette commands, so `tasks/pypilot.json` runs
`pypilot setup` / `pypilot doctor` in a terminal. Those (and `pypilot fix`,
`install`, `migrate-conda`, `migrate-to-pixi`) write a request file:

```
<cache dir>/rescan/<16 hex digits>.req      content: "<doctor|setup> <unix millis>"
```

`<cache dir>` is the platform cache dir for `dev.PyPilot.pypilot` (Linux
`~/.cache/pypilot`, Windows `%LOCALAPPDATA%\PyPilot\pypilot\cache`, macOS
`~/Library/Caches/dev.PyPilot.pypilot`), else `<temp>/pypilot-cache`. The
digits are Rust's `DefaultHasher` over the lowercased canonical workspace
path, so only the same binary produces the same name. The server polls the
file's mtime once a second and, when it changes, re-runs the onboarding
check as an explicit request (a clean result is confirmed), re-checks open
buffers and re-runs the workspace scan. Other editors get this for free by
running the same CLI commands.

## Editor setup

PyPilot's diagnostics only cover imports and the environment, so run it next
to your usual Python server (pyright, basedpyright, pylsp, ruff, ty), not
instead of it.

### Neovim (0.11+, `vim.lsp.config`)

```lua
vim.lsp.config('pypilot', {
  cmd = { 'pypilot', 'lsp' },
  filetypes = { 'python' },
  root_markers = { 'pyproject.toml', 'requirements.txt', 'pixi.toml', 'setup.py', '.git' },
  init_options = { diagnosticsScope = 'workspace' },
})
vim.lsp.enable('pypilot')
```

Use `init_options`: PyPilot never sends `workspace/configuration`, which is
how Neovim serves `settings`. Quick fixes come through
`vim.lsp.buf.code_action()`.

### Helix (`languages.toml`)

```toml
[language-server.pypilot]
command = "pypilot"
args = ["lsp"]
config = { diagnosticsScope = "workspace" }

[[language]]
name = "python"
# List the servers you already use as well; this array replaces the default.
language-servers = ["pyright", "pypilot"]
```

### Emacs (Eglot)

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '((python-mode python-ts-mode) . ("pypilot" "lsp"))))

;; Eglot sends this with workspace/didChangeConfiguration on connect, which
;; replaces initializationOptions (see Settings), so configure it here.
(setq-default eglot-workspace-configuration
              '(:pypilot (:diagnosticsScope "workspace")))
```

Eglot runs one server per buffer, so this entry replaces your other Python
server for those modes; put the two behind an LSP multiplexer if you want
both.

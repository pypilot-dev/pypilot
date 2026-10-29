# Configuration

PyPilot reads TOML settings from a global file and a per-project file, and takes settings from the editor in between.

## Files

| Scope | Path |
|---|---|
| Global (defaults for every project) | `config.toml` in the platform config directory: `~/.config/pypilot/config.toml` on Linux, `~/Library/Application Support/dev.PyPilot.pypilot/config.toml` on macOS, `%APPDATA%\PyPilot\pypilot\config\config.toml` on Windows |
| Project | `<project>/.zed/pypilot.toml`, or else `<project>/pypilot.toml` (the first that exists) |

A malformed file is ignored with a message on stderr, never an error. Unknown keys are ignored.

## Precedence

Per key, later layers win:

1. Built-in defaults
2. Global `config.toml`
3. The editor's settings (VS Code `pypilot.*`, sent over LSP) <Badge type="tip" text="1.3" />
4. The project's `pypilot.toml`

The project file wins because it is shared with everyone working on the project. An editor value equal to the built-in default counts as *unset* (VS Code sends every setting, defaults included, and they must not mask your global file). A key absent from a layer never clears a value from an earlier one.

Each `initializationOptions` or `workspace/didChangeConfiguration` the editor sends **replaces the whole editor layer**. A client that sends options at startup and then an empty or unrelated configuration change loses the first set, so an editor should send every key the same way. See [Editor settings over LSP](#editor-settings-over-lsp).

Command-line flags (`--path`, `--pixi-env`) apply to one run and override all of these.

| Editor | How its settings reach the engine |
|---|---|
| VS Code | `pypilot.*` settings, sent over LSP (layer 3) <Badge type="tip" text="1.3" />. VS Code 1.2.1 and older send none, so only the TOML files apply |
| Zed | The extension itself sends no PyPilot settings: use `.zed/pypilot.toml`, `pypilot.toml` or the global file |
| Neovim, Helix, Emacs | Whatever the client config sends as initialization options or workspace configuration; see [Other editors](./editors) |

## Keys

```toml
package_manager    = "auto"           # "auto", "uv", "uv-pip", "pip" or "pixi"
notifications      = "problems-only"  # "all", "problems-only" or "off"
auto_check_on_open = true
data_refresh_days  = 7                # 0 = never refresh the data tables
pixi_environment   = "default"        # which pixi environment is the project's
diagnostics_scope  = "openFiles"      # or "workspace": check every Python file

[state]
never = true                          # written by "Never for this project"
```

### package_manager

How PyPilot builds environments and installs packages.

| Value | Meaning |
|---|---|
| `auto` (default) | uv project mode when uv is on `PATH`; otherwise pip. It does not silently fetch a managed uv. |
| `uv` | A uv project: `pyproject.toml` + `uv.lock`, `uv init` / `uv add` / `uv sync`. Can install the Python it needs. |
| `uv-pip` | uv's pip interface on `requirements.txt`: `uv venv` + `uv pip install` + freeze. |
| `pip` | `python -m venv` + pip, then `pip freeze > requirements.txt`. Never downloads interpreters: it names the Python to install instead. |
| `pixi` <Badge type="tip" text="1.3" /> | Manage the project with pixi. On a project without a pixi manifest, setup first writes `pixi.toml` from the dependencies (as `pypilot migrate-to-pixi --write`), then runs `pixi install`. |

A pixi project (a `pixi.toml`, or `[tool.pixi.workspace]` in `pyproject.toml`) is always managed with pixi, whatever this says. VS Code: `pypilot.packageManager`.

### notifications

| Value | Meaning |
|---|---|
| `problems-only` (default) | Notify on warnings and errors. A correct project stays silent. |
| `all` | Also informational findings: the CUDA build picked, a conda project detected, pixi advice. |
| `off` | No notifications, and no import diagnostics either. |

VS Code: `pypilot.notifications`.

### auto_check_on_open

`true` (default): scan the project when the editor opens it. VS Code: `pypilot.autoCheckOnOpen`.

### data_refresh_days

How often, in days, the bundled data tables are refreshed from the repository (default `7`). `0` keeps PyPilot fully offline for tables; `pypilot update-data` still refreshes on demand. The same TTL caches the PyPI-to-conda mapping used by the pixi migration. VS Code: `pypilot.dataRefreshDays`.

### pixi_environment <Badge type="tip" text="1.3" />

Which pixi environment is the project's environment (`default` unless set): the one diagnostics, the installed-package scan and GPU checks look at, and that setup installs. Names are lowercase letters, digits and dashes; blank means unset. Ignored outside pixi projects. Override per run with `--pixi-env <name>`. VS Code: `pypilot.pixiEnvironment`.

### diagnostics_scope <Badge type="tip" text="1.3" /> {#diagnostics-scope}

Which files get import diagnostics in the editor.

| Value | Meaning |
|---|---|
| `openFiles` (default; `open-files` also accepted in TOML) | Only the files and notebook cells open in the editor |
| `workspace` | Also every `.py` file and notebook in the project, scanned in the background, so the Problems panel covers files you have not opened |

The scan skips hidden directories (`.venv`, `.pixi`, `.git`, …), `venv`, `env`, `node_modules`, `__pycache__`, `target`, `build`, `dist`, `site-packages`, any directory with a `pyvenv.cfg`, and what the project's `.gitignore` files exclude. It stops at 5,000 files. Open files always win over the scan. It has no effect on the CLI. Details: [LSP protocol](./lsp#workspace-diagnostics). VS Code: `pypilot.diagnosticsScope`.

### [state] never

Written by **Never for this project** into `.zed/pypilot.toml`. While `true`, the workspace notification is not shown for this project. Delete the line to undo.

## Editor settings over LSP <Badge type="tip" text="1.3" /> {#editor-settings-over-lsp}

Editors may send the same settings as JSON, in `initializationOptions` and in `workspace/didChangeConfiguration`. Keys are camelCase, either at the top level or under `pypilot`:

| JSON key | TOML key |
|---|---|
| `packageManager` | `package_manager` |
| `notifications` | `notifications` |
| `autoCheckOnOpen` | `auto_check_on_open` |
| `dataRefreshDays` | `data_refresh_days` |
| `pixiEnvironment` | `pixi_environment` |
| `diagnosticsScope` <Badge type="tip" text="1.3" /> | `diagnostics_scope` (editor values: `openFiles`, `workspace`) |

Values that do not parse are ignored with a log line. Keys the engine does not know (`autoSelectVenv` and other client-only settings) are ignored. The engine never asks for settings with `workspace/configuration`; the editor must push them. See [LSP protocol](./lsp#configuration).

## VS Code-only settings

`pypilot.autoSelectVenv` is handled by the extension itself. See [VS Code settings](/vscode/settings).

## Examples

A pip-only team project, quiet:

```toml
# pypilot.toml, committed
package_manager = "pip"
notifications   = "off"
```

An air-gapped machine:

```toml
# ~/.config/pypilot/config.toml
data_refresh_days = 0
```

Import diagnostics for the whole project, for everyone on it:

```toml
# pypilot.toml
diagnostics_scope = "workspace"
```

A pixi project whose GPU environment is the one to check:

```toml
# pypilot.toml
pixi_environment = "cuda"
```

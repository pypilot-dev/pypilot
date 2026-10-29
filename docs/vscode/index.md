# VS Code

The VS Code extension runs the same engine as Zed and the CLI, and adds VS Code surfaces: the command palette, a status bar item, progress notifications, a report panel, tasks, and automatic interpreter and kernel selection.

- [Marketplace](https://marketplace.visualstudio.com/items?itemName=AbdullahMasood-005.pypilot-vscode) · [Open VSX](https://open-vsx.org/extension/AbdullahMasood-005/pypilot-vscode) · [Source](https://github.com/pypilot-dev/pypilot-vscode)
- Requires VS Code 1.85 or later. Current release: **1.3.0**, which pins engine 1.3.0. Items marked <Badge type="tip" text="1.3" /> are new in 1.3.0.

## What happens when you open a folder

The extension activates in folders that contain Python files, `requirements.txt`, `pyproject.toml`, `pixi.toml` or `environment.yml`, and when a Jupyter notebook opens. Then:

1. **It finds the engine**: a `pypilot` on `PATH` or its own downloaded copy (the newer of the two), else it downloads the engine version it pins (about 3 MB) and verifies it, see [below](#a-pinned-verified-engine). With `pypilot.helperPath` set, it uses that binary instead. See [how wrappers pick an engine](/guide/ecosystem#vs-code).
2. **It starts the language server** (`pypilot lsp`) for Python files, notebook cells, and `requirements.txt`, `pyproject.toml` and `environment.yml`.
3. **It scans the project** (unless `pypilot.autoCheckOnOpen` is off). If something needs attention, one notification offers **Fix environment**, **Show details**, **Ignore** and **Never for this project**.
4. **It selects the interpreter.** With `pypilot.autoSelectVenv` on, the project's `.venv` becomes the active Python interpreter (and so the recommended notebook kernel) when it is created or changes, and on startup when no interpreter inside the workspace is selected.

## Status bar

A **PyPilot** item sits to the left of the Python interpreter picker:

| Shows | Meaning | Click |
|---|---|---|
| `✓ PyPilot` | Environment looks good | Opens the doctor report |
| `⚠ PyPilot: …` (warning background) | Something needs attention | Runs setup |
| `⊗ PyPilot: …` (error background) | Environment error | Runs setup |
| `PyPilot: checking…` (spinning) | Scan in progress | none |
| `⚠ PyPilot`, no command | The engine could not be found or downloaded | See the error notification |

<Badge type="tip" text="1.3" /> The tooltip shows the environment's Python version and path, the package manager PyPilot uses (`uv`, `uv-pip`, `pip` or `pixi`, marked "auto" when chosen automatically) and, in pixi projects, the pixi environment. It reads files on disk and starts no process.

## The report

**PyPilot: Doctor: Show Environment Report** (or a click on the status bar) runs the same assessment as `pypilot doctor` and opens it as a Markdown document.

<Badge type="tip" text="1.3" /> **PyPilot: Open Environment Report Panel** shows it in a webview panel with colour-coded findings and fix buttons (Fix CUDA only for projects that use torch or tensorflow), plus **Copy as Markdown** and **Save as HTML** (a standalone page with inline styles in your theme's colours, no scripts). The Markdown report gets an editor title button that opens the panel.

## Quick fixes on imports

Imports that are not installed, or that cannot run on the environment's Python, get a squiggle. `Ctrl+.` offers to install the package (naming the distribution when the import name differs, such as `cv2` for `opencv-python`) or to recreate the environment on a Python that supports it. Select several imports to install them in one step, or use **Install all N missing packages in this file** (or *in this notebook*).

<Badge type="tip" text="1.3" /> With `pypilot.diagnosticsScope` set to `workspace`, every Python file is checked in the background, so the Problems panel covers files you have not opened.

## pixi projects

A folder with a `pixi.toml`, or a `pyproject.toml` with `[tool.pixi.workspace]`, is a pixi project. The engine manages it through pixi (since engine 1.2.0): no `.venv`, `pixi.lock` untouched, installs through `pixi add --pypi`.

<Badge type="tip" text="1.3" /> The extension also:

- selects `.pixi/envs/<name>` as the interpreter, where `<name>` is [`pypilot.pixiEnvironment`](./settings#pypilot-pixienvironment) (`default` unless set); changing the setting switches right away;
- activates in folders that only contain a `pixi.toml`, and tells the engine when it changes;
- adds **PyPilot: Install pixi** and **PyPilot: Initialize pixi project** and a `pixi` value for `pypilot.packageManager`.

See the [pixi guide](/pixi/).

## Multi-root workspaces <Badge type="tip" text="1.3" />

In a window with several folders, PyPilot runs one language server per folder that holds a Python project (`pyproject.toml`, `requirements.txt`, `pixi.toml`, `environment.yml`, `setup.py`, `setup.cfg`, `Pipfile`, or any `.py` / `.ipynb` file); a folder without one gets a server when you open a Python file from it.

- Each server checks its own folder with that folder's `pypilot.*` settings and environment; interpreter selection works per folder.
- Commands act on the active editor's folder, or ask which one; quick fixes go to the file's folder; tasks are listed per folder.
- A folder nested in another workspace folder is handled by the outer one.
- You may see one notification per folder on startup.

## Walkthrough <Badge type="tip" text="1.3" />

**Get started with PyPilot** (Help > Welcome): open a Python project, see what PyPilot checks, fix the environment, and what is different in pixi projects.

## A pinned, verified engine <Badge type="tip" text="1.3" />

Since 1.3.0 the extension no longer tracks "latest":

- It installs the engine version named in its `package.json` (`pypilotEngine`, engine 1.3.0 in extension 1.3.0) and checks every download's SHA-256 before extracting it. Mismatches, and releases without `SHA256SUMS`, install nothing. See [Security](/guide/security#what-each-wrapper-downloads-and-checks).
- [`pypilot.engineUpdates`](./settings): `pinned` (default) offers only the pinned version, so updating the extension brings newer engines; `latest` also offers newer GitHub releases, verified against their `SHA256SUMS`.
- [`pypilot.helperPath`](./settings): run exactly this binary (for example a local `cargo build`); nothing is downloaded and no updates are offered while it is set.
- Resolution order: `pypilot.helperPath`; else the newer of a `pypilot` on `PATH` and the extension's copy; else download the pinned version.
- Behind a proxy, VS Code's `http.proxy` (or the system proxy) is used; with `http.proxySupport` set to `off`, `HTTPS_PROXY` / `NO_PROXY` are honoured.
- The **PyPilot** output channel logs which engine is used and which checksum each download matched.

## If the language server stops

VS Code restarts a crashed language server a few times, then gives up. If PyPilot stopped answering (no squiggles, commands that do nothing), for example because its process was killed or you replaced the binary, run **Developer: Reload Window**. The **PyPilot** output channel shows what happened, and <Badge type="tip" text="1.3" /> `pypilot logs --tail` shows what the engine last ran. See [Troubleshooting](/guide/troubleshooting#the-editor-stopped-responding).

Next: [Commands & tasks](./commands) · [Settings](./settings)

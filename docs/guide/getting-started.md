# Getting started

Pick the place you work. Every option runs the same engine, so you can mix them: the VS Code extension and the Zed extension both prefer a `pypilot` already on your `PATH`.

## Zed

1. Open the extensions panel (`zed: extensions`) and install **PyPilot**.
2. Open a Python folder. The extension downloads the engine for your platform and starts it as a language server.
3. If something needs attention you get one notification with **Fix environment**, **Show details**, **Ignore** and **Never for this project**.

Zed extensions cannot add command-palette entries, so the commands ship as tasks: copy [`tasks/pypilot.json`](https://github.com/pypilot-dev/pypilot/blob/main/tasks/pypilot.json) into `.zed/tasks.json` (or your global tasks) and run them with `task: spawn`. More in [Zed](/zed/).

## VS Code

1. Install **PyPilot** from the [Marketplace](https://marketplace.visualstudio.com/items?itemName=AbdullahMasood-005.pypilot-vscode) or [Open VSX](https://open-vsx.org/extension/AbdullahMasood-005/pypilot-vscode).
2. Open a Python folder. The extension finds or downloads the engine (about 3 MB) and scans the project.
3. Follow the prompt, or run **PyPilot: Set Up Environment** from the command palette. The **Get started with PyPilot** walkthrough (Help > Welcome) covers the same steps.

Requires VS Code 1.85 or later. More in [VS Code](/vscode/).

## pip, uv or pipx

```bash
pip install pypilot-cli
# or, as an isolated tool
uv tool install pypilot-cli
pipx install pypilot-cli
```

The package installs a `pypilot` command. On first run it downloads the engine release it was built for into your cache directory. Supports Python 3.8 and later. More in [Python package](/python/).

## cargo (from source)

```bash
git clone https://github.com/pypilot-dev/pypilot.git
cd pypilot
cargo install --path helper --locked
```

This puts `pypilot` in `~/.cargo/bin`, which both editor extensions pick up. See [Building from source](/engine/building).

## Your first run

In a project folder:

```bash
pypilot doctor     # read-only report: what Python, what's wrong, what to do
pypilot setup      # build the environment on the recommended Python
```

<TerminalDemo src="setup" cmd="pypilot setup" title="~/src/fresh" />

`setup` chose Python 3.15 because every dependency has wheels for it, installed that interpreter with uv, created `.venv`, and moved `requirements.txt` into a new `pyproject.toml` (the `requirements.txt` file is left in place). From here, `pypilot install <package>` adds dependencies the same way.

::: tip uv is optional
With uv on `PATH` (the default `auto` mode) PyPilot uses a uv project and can install the Python it needs. Without uv it uses `python -m venv` and pip, and tells you which Python to install when the right one is missing. See [`package_manager`](/reference/configuration#package-manager).
:::

## Platforms

Prebuilt engines exist for Linux x64, macOS x64, macOS arm64 (Apple Silicon) and Windows x64. On other platforms, [build from source](/engine/building) and put `pypilot` on your `PATH`.

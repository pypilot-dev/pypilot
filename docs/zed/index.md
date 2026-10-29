# Zed

The Zed extension is a small WebAssembly shim (`extension/` in the [engine repo](https://github.com/pypilot-dev/pypilot)). It finds or downloads the engine and registers it with Zed as a language server for Python. Everything else happens in the engine.

## Install

Open the extensions panel (`zed: extensions`), search for **PyPilot**, install. Then open a Python folder.

The shim uses a `pypilot` on your `PATH` when its version is new enough; otherwise it downloads the newest release asset for your platform from `github.com/pypilot-dev/pypilot` and caches it per version. If GitHub cannot be reached it falls back to a copy it downloaded earlier. Its only declared capability is downloading from that repository's releases.

## What you see

- **One notification on open** when something needs attention, with **Fix environment**, **Show details**, **Ignore** and **Never for this project**. A correct environment stays silent. <Badge type="tip" text="1.3" /> When pixi would help, an **Initialize pixi project** button is added; the engine runs a dry run, shows one message with what `pixi.toml` would contain, and offers **Write and install**, **Write pixi.toml** or **Cancel**; nothing is written before you choose (see [From the editor](/pixi/migrate#from-the-editor)).
- **Diagnostics on imports** in Python files, with code actions (`ctrl-.`) to install the package or recreate the environment on a supported Python.
- **Show details** opens the full report as a Markdown document.
- **Pointing Zed's Python server at the environment.** If Zed's own Python language server keeps reporting installed packages as missing because it looks at another interpreter, PyPilot offers to pin pyright to the project environment (in `pyproject.toml` under `[tool.pyright]`, or a new `pyrightconfig.json`). This is offered in Zed only, and only after you agree is anything written.

## Commands as tasks

Zed extensions cannot add command-palette entries, so PyPilot's commands ship as tasks. Copy [`tasks/pypilot.json`](https://github.com/pypilot-dev/pypilot/blob/main/tasks/pypilot.json) into your project's `.zed/tasks.json`, or into your global tasks file (`~/.config/zed/tasks.json`; on Windows `%APPDATA%\Zed\tasks.json`). Run them with `task: spawn`.

| Task | Runs |
|---|---|
| `pypilot: fix environment (set up environment)` | `pypilot setup` |
| `pypilot: show details (doctor report)` | `pypilot doctor` |
| `pypilot: explain python range (what blocks each version)` <Badge type="tip" text="1.3" /> | `pypilot explain` |
| `pypilot: fix python version` | `pypilot fix python` |
| `pypilot: fix cuda (re-pin torch/tensorflow to the driver)` | `pypilot fix cuda` |
| `pypilot: update data (refresh bundled tables)` | `pypilot update-data` |
| `pypilot: migrate conda to pyproject.toml` | `pypilot migrate-conda` |
| `pypilot: preview migration to pixi (pixi.toml)` <Badge type="tip" text="1.3" /> | `pypilot migrate-to-pixi` |
| `pypilot: migrate to pixi (write pixi.toml and install)` <Badge type="tip" text="1.3" /> | `pypilot migrate-to-pixi --write --install` |
| `pypilot: install pixi` <Badge type="tip" text="1.3" /> | `pypilot install-pixi` |

The tasks pass no `--path`: Zed runs them in the worktree root, so paths with spaces are never split. A task run of `doctor` or `setup` also tells the running language server to re-scan, so the editor notification reflects what the terminal just did.

::: tip Windows: "os error 193" when spawning a task
Zed's spawner cannot launch Microsoft Store *App Execution Aliases*, including a Store-installed `pwsh.exe`. Point the task shell at a real executable, for example:

```json
"shell": {
  "with_arguments": {
    "program": "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
    "args": ["-NoProfile"]
  }
}
```

`-NoProfile` also roughly halves the spawn time.
:::

## Configuration

Zed has no settings UI for the engine; use a `pypilot.toml`:

```toml
# .zed/pypilot.toml (or pypilot.toml) in the project root
package_manager    = "uv"
notifications      = "problems-only"
auto_check_on_open = true
data_refresh_days  = 7
diagnostics_scope  = "workspace"     # 1.3: import diagnostics for every file, not just open ones
```

The Zed extension itself sends no PyPilot settings over LSP, so set them in this file or in the global `config.toml`. All keys are in the [configuration reference](/reference/configuration). <Badge type="tip" text="1.3" /> In Zed, `pypilot completions`, `pypilot logs --tail` and `pypilot setup --dry-run` work in the terminal next to the tasks; see [Logs, version & completions](/engine/logs) and [Dry runs](/engine/dry-run).

## Develop the extension

```bash
cargo install --path helper --locked    # the shim prefers a pypilot on PATH
rustup target add wasm32-wasip2
```

Then in Zed run `zed: install dev extension` and select the `extension` directory.

## Registry

The extension is listed in [zed-industries/extensions](https://github.com/zed-industries/extensions) as `pypilot-language-server`. The registry entry points at an engine release commit, so it is updated after each engine release.

# Zed

The Zed extension is a small WebAssembly shim (`extension/` in the [engine repo](https://github.com/pypilot-dev/pypilot)). It finds or downloads the engine and registers it with Zed as a language server for Python. Everything else happens in the engine.

## Install

Open the extensions panel (`zed: extensions`), search for **PyPilot**, install. Then open a Python folder.

The shim uses a `pypilot` on your `PATH` when its version is new enough; otherwise it downloads the newest release asset for your platform from `github.com/pypilot-dev/pypilot` and caches it per version. If GitHub cannot be reached it falls back to a copy it downloaded earlier. Its only declared capability is downloading from that repository's releases.

::: danger Windows and macOS may block the engine
The engine is not code signed yet. **Windows 11 Smart App Control** blocks `pypilot.exe` with no per-app exception: turn it off in Windows Security > App & browser control > Smart App Control settings. **Microsoft Defender SmartScreen** or **macOS Gatekeeper** can also warn about a copy you downloaded yourself. See [Smart App Control](/guide/troubleshooting#smart-app-control), [SmartScreen and Defender](/guide/troubleshooting#defender) and [macOS](/guide/troubleshooting#macos-gatekeeper).
:::

## What you see

- **One notification on open** when something needs attention, with **Fix environment**, **Show details**, **Ignore** and **Never for this project**. A correct environment stays silent. <Badge type="tip" text="1.3" /> When pixi would help, an **Initialize pixi project** button is added; the engine runs a dry run, shows one message with what `pixi.toml` would contain, and offers **Write and install**, **Write pixi.toml** or **Cancel**; nothing is written before you choose (see [From the editor](/pixi/migrate#from-the-editor)).
- **Diagnostics on imports** in Python files, with code actions (`ctrl-.`) to install the package or recreate the environment on a supported Python.
- **Show details** opens the full report as a Markdown document.
- **Pointing Zed's Python server at the environment.** If Zed's own Python language server keeps reporting installed packages as missing because it looks at another interpreter, PyPilot offers to pin pyright to the project environment (in `pyproject.toml` under `[tool.pyright]`, or a new `pyrightconfig.json`). This is offered in Zed only, and only after you agree is anything written.

## Commands as tasks

Zed extensions cannot add command-palette entries, so PyPilot's commands ship as tasks. You add them once:

1. Open the command palette (`ctrl-shift-p`, or `cmd-shift-p` on macOS), type `zed: open tasks` and press Enter. This opens your global tasks file (`%APPDATA%\Zed\tasks.json` on Windows, `~/.config/zed/tasks.json` on macOS and Linux).

   ![The command palette with "zed: open tasks" selected](/zed-open-tasks.png)

2. A new file holds Zed's default comments and an example task. Delete all of it (`ctrl-a`, then Delete), or comment it out.
3. Paste the template for your OS: [`tasks/windows.json`](https://github.com/pypilot-dev/pypilot/blob/main/tasks/windows.json) on Windows, [`tasks/macos-linux.json`](https://github.com/pypilot-dev/pypilot/blob/main/tasks/macos-linux.json) on macOS and Linux. Open the link, click **Raw**, copy everything and save the file.

That's it. To run a task:

1. Open the command palette again and choose `task: spawn` (or press `alt-shift-t`).

   ![The command palette with "task: spawn" selected](/zed-task-spawn.png)

2. Pick any of the `pypilot:` entries and press Enter. It runs in Zed's terminal.

   ![The task picker with the pypilot tasks highlighted](/zed-task-list.png)

If you already have tasks of your own, keep them and paste only the entries between PyPilot's `[` and `]` into your list. To add the tasks to one project only, use `zed: open project tasks` instead, which opens `.zed/tasks.json`.

Each task runs the `pypilot` on your `PATH` if there is one, and otherwise the copy the extension downloaded for its language server, so the tasks work on a fresh machine without installing the CLI. Open a Python file once first, so the extension has downloaded it. <Badge type="tip" text="1.4" /> The fix environment task runs `pypilot doctor --fix`, which applies only the fixes doctor found (see [What doctor --fix fixes](/guide/fixing)); the set up task runs the full `pypilot setup`.

| Task | Runs |
|---|---|
| `pypilot: fix environment (only what doctor found)` | `pypilot doctor --fix` |
| `pypilot: set up environment (full bootstrap)` | `pypilot setup` |
| `pypilot: show details (doctor report)` | `pypilot doctor` |
| `pypilot: explain python range (what blocks each version)` <Badge type="tip" text="1.3" /> | `pypilot explain` |
| `pypilot: fix python version` | `pypilot fix python` |
| `pypilot: fix cuda (re-pin torch/tensorflow to the driver)` | `pypilot fix cuda` |
| `pypilot: fix requirements (apply doctor's requirement edits)` <Badge type="tip" text="1.4" /> | `pypilot fix requirements` |
| `pypilot: update data (refresh bundled tables)` | `pypilot update-data` |
| `pypilot: migrate conda to pyproject.toml` | `pypilot migrate-conda` |
| `pypilot: preview migration to pixi (pixi.toml)` <Badge type="tip" text="1.3" /> | `pypilot migrate-to-pixi` |
| `pypilot: migrate to pixi (write pixi.toml and install)` <Badge type="tip" text="1.3" /> | `pypilot migrate-to-pixi --write --install` |
| `pypilot: install pixi` <Badge type="tip" text="1.3" /> | `pypilot install-pixi` |

The tasks pass no `--path`: Zed runs them in the worktree root, so paths with spaces are never split. A task run of `doctor`, `doctor --fix` or `setup` also tells the running language server to re-scan, so the editor notification reflects what the terminal just did.

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

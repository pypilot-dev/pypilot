# Logs, version & completions <Badge type="tip" text="next" />

Three small commands that help when something goes wrong, or when you just want tab completion. All three ship in the next engine release.

## The local log

Every CLI command, and every LSP command or workspace scan, appends one JSON line to `pypilot.log`. It records what PyPilot actually ran: the command, the workspace, the mode, every subprocess with its exit code and duration, and the stderr of the ones that failed. **Nothing is sent anywhere.**

| Platform | Location |
|---|---|
| Windows | `%LOCALAPPDATA%\PyPilot\pypilot\cache\logs\pypilot.log` |
| macOS | `~/Library/Caches/dev.PyPilot.pypilot/logs/pypilot.log` |
| Linux | `~/.cache/pypilot/logs/pypilot.log` |

The file rotates at 1 MB and three files are kept (`pypilot.log`, `pypilot.log.1`, `pypilot.log.2`). Writing it is best-effort: a log that cannot be written is skipped, and never breaks a command.

| Variable | Effect |
|---|---|
| `PYPILOT_NO_LOG=1` | Turn the log off |
| `PYPILOT_LOG_DIR=<dir>` | Write it to `<dir>` instead |

### Reading it

```bash
pypilot logs                  # the log's path
pypilot logs --tail 5         # the last 5 entries, summarized
pypilot logs --tail --json    # the last 10 entries as raw JSON lines
```

`pypilot logs --tail 5`, after a `doctor`, a `setup --dry-run`, two `check`s (one of them a misspelt name) and an `install --dry-run`:

<<< @/snippets/logs-tail.txt{text}

Each summary shows the time (UTC), the source (`cli` or `lsp`), the command, the result, the duration, the workspace, the configured mode and, after `->`, the mode it resolved to, then how many subprocesses ran. The step count includes the read-only probes, such as looking for interpreters; on this machine the Windows `py` launcher has no 3.12, 3.13 or 3.15, so those probes fail, which is normal and is why a failed entry lists them. For a failed entry, the last three failed steps and the error follow.

### Fields

One line, pretty-printed:

```json
{
  "ts": "2026-10-29T13:40:50.887Z",
  "pypilot": "1.2.2",
  "source": "cli",
  "command": "install",
  "args": ["httpx", "--dry-run"],
  "workspace": "C:\\src\\demo",
  "mode": "Auto",
  "resolved_mode": "UvFull",
  "ok": true,
  "duration_ms": 33,
  "steps": [
    { "cmd": "uv", "args": ["--version"], "ok": true, "exit": 0, "duration_ms": 15 },
    { "cmd": "uv", "args": ["--version"], "ok": true, "exit": 0, "duration_ms": 13 }
  ],
  "error": null
}
```

| Field | Meaning |
|---|---|
| `ts` | When the command started, UTC |
| `pypilot` | Engine version |
| `source` | `cli` or `lsp` |
| `command`, `args` | The mode (or LSP action) and its arguments |
| `workspace` | The project folder |
| `mode`, `resolved_mode` | The `package_manager` setting and the mode it resolved to (`null` when none was needed). Written as `Auto`, `UvFull` and so on, not the snake_case of the JSON output |
| `ok`, `error` | The result, and the error message when it failed |
| `duration_ms` | Wall time |
| `steps` | Every subprocess: `{ cmd, args, ok, exit, duration_ms, stderr }`. `stderr` holds the last 2,000 characters, for failed steps only. At most 100 steps are kept per entry; the rest are only counted |

### What is masked

Arguments, stderr and errors pass through a redactor before they are written: credentials in URLs (`https://user:token@host`), `NAME=value` pairs and `--name value` flags whose name contains `token`, `secret`, `password`, `passwd`, `auth`, `credential` or `api-key`/`api_key`/`apikey`, and well-known token formats (GitHub `ghp_`/`github_pat_`…, PyPI `pypi-`, GitLab `glpat-`, Slack `xox…`, `sk-` keys). The environment is never recorded. Still, read a log before you paste it into a public issue: paths and package names are in it.

## Version

```bash
pypilot --version          # pypilot 1.2.2
pypilot version            # also commit, target and data tables
pypilot --version --json   # everything, as JSON
```

<<< @/snippets/version.txt{text}

Bare `--version` stays one line forever: the Zed and VS Code extensions and `pypilot-cli` parse it. The `data` line says when the bundled tables were last changed in the source, how long ago the cache was refreshed and when the next refresh is due; the table below it shows, per table, its schema and whether the bundled copy or a refreshed one is in use. For the JSON fields see [Machine-readable output](./output#version-json).

## Is the data current?

`pypilot doctor` shows the tables' state offline, as a line under **Environment**:

```text
  data tables     : refreshed just now (next in 6 days); 1 of 5 bundled
```

`pypilot update-data --check` goes online and compares the copies in use with `main`, writing nothing. It exits `0` when current, `1` when stale, `2` when a fetch failed. See [`update-data --check`](./commands#update-data-check).

## Shell completion

`pypilot completions <shell>` prints a script that completes modes, flags, `fix python|cuda`, `--to` values and pixi platform names.

::: code-group

```bash [bash]
# ~/.bashrc
source <(pypilot completions bash)
```

```zsh [zsh]
# ~/.zshrc
source <(pypilot completions zsh)
```

```fish [fish]
pypilot completions fish > ~/.config/fish/completions/pypilot.fish
```

```powershell [PowerShell]
# $PROFILE
pypilot completions powershell | Out-String | Invoke-Expression
```

:::

The PowerShell script registers a native argument completer for `pypilot` and `pypilot.exe`. It starts like this:

<<< @/snippets/completions-powershell.txt{powershell}

With `pypilot-cli`, the wrapper passes `completions` straight to the engine, so the same commands work.

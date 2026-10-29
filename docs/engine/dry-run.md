# Dry runs <Badge type="tip" text="1.3" />

Every command that changes your environment can show what it would do first. `--dry-run` prints the plan: each step in order, its exact command line, the files it would create or change, and why, under the solver's reason for the chosen Python. Nothing is executed, written or downloaded.

```bash
pypilot setup --dry-run
pypilot install httpx --dry-run
pypilot fix python --dry-run
pypilot fix cuda --dry-run
pypilot doctor --fix --dry-run      # the report, then the plan "Fix environment" would run
pypilot setup --dry-run --json      # the plan as JSON
```

`--dry-run` needs engine 1.3.0 or newer; 1.2.2 and older do not have it.

## What a plan shows

<TerminalDemo src="setup-dry-run" cmd="pypilot setup --dry-run" title="~/src/demo" />

- The header names the mode (`uv`, `uv-pip`, `pip` or `pixi`) and the Python the run would use, then the solver's reason for it.
- Lines with `•` are the read-only checks the real run makes first (here: is uv on `PATH`). They run during a dry run too, because the plan depends on them.
- Each numbered step is a process the real run would start, with `$` and its argument vector, `writes:` (paths relative to the project) and `why:`. A step that is not a process (writing the PyTorch index into `pyproject.toml`, downloading uv, the GPU self-test) has no `$` line.
- `Nothing to do.` when the environment already matches, for example `fix cuda` in a project without torch or tensorflow.

## Why you can trust it

The plan is not a separate description of what the command does. The command runs its own code path with every change recorded instead of made: each subprocess, file write and download goes through one executor that, in a dry run, notes the step and returns. So the plan is what the real run does on this machine, today, with these settings. Only the probes and PyPI lookups the analysis always makes actually run.

What it cannot know in advance: the result of a step that never ran. If the real `uv add` would fail on a resolver conflict, the plan still lists `uv add`; [`pypilot doctor`](./commands#doctor) and the uv dry-run finding are where such conflicts show up.

## When the real run would stop

If the run would stop before it finishes, the plan says where, and the command exits with status `1`. Here pip mode needs a Python that is not installed, and pip cannot fetch one:

<<< @/snippets/setup-dry-run-blocked.txt{text}

In scripts, the exit status is the quickest check: `0` means the plan runs to the end.

## JSON

With `--json` the plan replaces the command's usual document. `command` is the command as typed (`doctor --fix` for that one), `dry_run` is `true`, and `ok` is `true` exactly when `plan.blocked` is `null`:

| Key | Meaning |
|---|---|
| `plan.mode` | A `resolved_mode` value: `uv_full`, `uv_pip_compat`, `pure_pip` or `pixi` |
| `plan.python` | The Python the run would use, or `null` |
| `plan.why` | The solver's reason, as in the text header |
| `plan.steps` | `[{ "name", "command": [argv], "writes": [paths], "why" }]`. `command` is the exact argument vector, program first; empty for a step that is not a process. `writes` holds absolute paths |
| `plan.notes` | The read-only checks: `[{ "name", "ok", "detail" }]` |
| `plan.blocked` | Why the real run would stop, else `null` |

`pypilot setup --dry-run --json` for the project above:

<<< @/snippets/setup-dry-run-json.json{json}

## In the editors

The editors' buttons (**Fix environment**, quick fixes) run the same code without `--dry-run`. To see what one of them would do, run the matching command with `--dry-run` in the project folder: **Fix environment** is `pypilot setup` (or `pypilot doctor --fix`), an install quick fix is `pypilot install <package>`, and `pypilot fix python` shows the rebuild onto the recommended Python that a "Rebuild environment on Python X.Y" quick fix starts with.

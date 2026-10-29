# Command reference

Every mode and flag of engine 1.3.0. Items marked <Badge type="tip" text="1.3" /> are new in 1.3.0; 1.2.2 and older have the rest. Output below was captured on Windows x64, with a build of `main` from just before the 1.3.0 version bump (so it still reports 1.2.2), an NVIDIA RTX A5000 laptop GPU and uv 0.13.5; project paths are shortened to `C:\src\…`. On your machine the versions PyPI reports will differ.

```text
pypilot <MODE> [ARG] [--path <dir>]
```

## Global options

| Option | Applies to | Meaning |
|---|---|---|
| `--path <dir>`, `-p <dir>`, `--path=<dir>` | all project modes | Project folder to analyze. Default: the current directory. If the folder's only Python code lives in one subfolder, that subfolder is used. |
| `--pixi-env <name>`, `--pixi-env=<name>` <Badge type="tip" text="1.3" /> | all project modes | Which pixi environment counts as the project's, for this run only. Default: the [`pixi_environment`](/reference/configuration#pixi-environment) setting (`default`). |
| `--json` <Badge type="tip" text="1.3" /> | every mode except `lsp` and `completions` | Print one JSON document on stdout instead of text (`--format=json` too); `logs --json` prints raw log lines instead. See [Machine-readable output](./output). |
| `--markdown`, `--md` <Badge type="tip" text="1.3" /> | `doctor`, `check` | Print the Markdown report the editors show under **Show details** (`--format=markdown` too). When both are given, `--json` wins. |
| `--dry-run` <Badge type="tip" text="1.3" /> | `setup`, `install`, `fix python`, `fix cuda`, `doctor --fix` | Print the plan (every command, the files it writes, and why) and change nothing. See [Dry runs](./dry-run). |
| `--version`, `-V` | none | Print `pypilot 1.3.0`, one line. Wrappers parse it, so it stays that way. |
| `version`, `--version --verbose` <Badge type="tip" text="1.3" /> | none | Add the commit, target and data-table state; `--json` for all of it. See [`version`](#version). |
| `--help`, `-h`, `help` | none | Print usage. |

With no mode at all, `pypilot` runs `doctor`. An unknown mode prints the usage and exits with status 1.

**Exit status.** `0` when the command ran, `1` when it could not (bad arguments, a package not on PyPI, a failed install, or a [dry run](./dry-run) whose plan would stop). `doctor` exits `0` even when it reports problems: read the findings, or use the [JSON output](./output) for scripting. `update-data --check` has its own codes: `0` current, `1` stale, `2` a fetch failed.

## doctor

Read-only report on the environment and the project's compatibility. Nothing is installed or written, except a small request file in the cache directory that tells a running language server to refresh its notification.

```bash
pypilot doctor [--json | --markdown] [--fix [--dry-run]] [--path <dir>] [--pixi-env <name>]
```

| Flag | Meaning |
|---|---|
| `--json`, `--markdown` <Badge type="tip" text="1.3" /> | The same report as one JSON document, or as the Markdown the editors show. See [Machine-readable output](./output). |
| `--fix` <Badge type="tip" text="1.3" /> | Print the report, then run what the editor's **Fix environment** runs (`setup`). With `--json` only setup's document is printed, with `command: "doctor --fix"`. |
| `--fix --dry-run` <Badge type="tip" text="1.3" /> | Print the report, then the plan `--fix` would run. See [Dry runs](./dry-run). |

Sections: **Environment** (uv, the project environment, pixi, interpreters found, and <Badge type="tip" text="1.3" /> the data tables' refresh state), **Project** (files, `requires-python`, dependencies), **pixi** (in pixi projects: platforms, lock status, environments), **Compatibility** (the supported range, the recommendation, each dependency's range) and **Findings**.

<TerminalDemo src="doctor" cmd="pypilot doctor" title="~/src/demo" />

When no Python satisfies every dependency:

<<< @/snippets/doctor-conflict.txt{text}

The [Findings reference](/reference/checks) lists everything doctor can report.


## check

Can one package run on this project's Python? Read-only. Takes a name or any requirement a requirements file accepts; a specifier is judged on the release it selects.

```bash
pypilot check <package> [--suggest-alternatives] [--json | --markdown] [--path <dir>]
```

| Flag | Meaning |
|---|---|
| `--suggest-alternatives` <Badge type="tip" text="1.3" /> | Show the conda-forge alternative section even when the verdict passes. On a failure it is always shown. |

<<< @/snippets/check.txt{text}

<Badge type="tip" text="1.3" /> When the verdict is a failure (no files for this platform, no wheel for this Python, a source build), check says why and names the conda-forge build pixi could install instead:

<<< @/snippets/check-gdal.txt{text}

<Badge type="tip" text="1.3" /> When the package shares no Python with one of the project's own dependencies, check names that dependency and both ranges (the JSON `conflict` field), so you know which pin to change.

A name that is not on PyPI exits with status 1: ``pypilot check: `nosuchpkg-zzqq` is not on PyPI; check the spelling``.

## explain <Badge type="tip" text="1.3" /> {#explain}

Why these Pythons. With a package: `requires_python` against the wheels it publishes for each platform, and which side sets each bound. Without one: every dependency's range and what blocks each Python version in the project.

```bash
pypilot explain [package] [--json] [--path <dir>]
```

| Flag | Meaning |
|---|---|
| `--json` | Print the explanation as JSON instead of tables. |

<TerminalDemo src="explain-pkg" cmd="pypilot explain mediapipe==0.10.14" />

<<< @/snippets/explain-project.txt{text}

The JSON form starts like this (trimmed); the `kind` is `project` or `package`:

```json
{
  "pypilot": "1.3.0",
  "kind": "project",
  "explanation": {
    "platform": "Windows x64",
    "requires_python": null,
    "supported": ["3.12"],
    "supported_range": "3.12",
    "recommended": "3.12",
    "recommendation": "Python 3.12",
    "binding_python": "3.13",
    "binding": [
      { "package": "mediapipe", "version": "0.10.14", "reason": "no_wheel", "detail": "wheels end at cp312" }
    ],
    "packages": [ { "name": "mediapipe", "version": "0.10.14", "requires_python": null,
                    "supported": ["3.9", "3.10", "3.11", "3.12"], "supported_range": "3.9–3.12", "note": "" } ],
    "pythons": [ { "python": "3.7", "ok": false, "blockers": [ "…" ] } ]
  }
}
```

## setup

Build the project's environment on the recommended Python, honouring [`package_manager`](/reference/configuration#package-manager). Same code path as the editor's **Fix environment** button.

```bash
pypilot setup [--dry-run] [--json] [--path <dir>] [--pixi-env <name>]
```

What it does, by mode:

| Mode | Steps |
|---|---|
| uv (`auto` with uv on `PATH`, or `uv`) | `uv init --bare` if there is no `pyproject.toml` (with `requires-python` and `.python-version` set to the chosen Python); `uv python install X.Y` if needed; `uv venv`; `uv add -r requirements.txt` when a requirements file exists, so its dependencies move into `pyproject.toml`. The chosen Python is pinned so a later `uv sync` keeps it. |
| `uv-pip` | uv's pip interface on `requirements.txt`: `uv venv` + `uv pip install`; no `pyproject.toml` is created. |
| `pip` (`auto` without uv, or `pip`) | `python -m venv` + `pip install`, then `pip freeze > requirements.txt`. It never downloads an interpreter: if the right Python is missing, it tells you which one to install. |
| pixi project | `pixi install`. No `.venv`, no uv, `pixi.lock` is pixi's. |
| `pixi` on a project without a pixi manifest <Badge type="tip" text="1.3" /> | Writes `pixi.toml` from the dependencies (as `migrate-to-pixi --write`), then `pixi install`. |

In projects with notebooks, setup also makes sure the environment has `ipykernel>=6.31.0,<7` (see [Notebooks](/guide/notebooks)). With an NVIDIA GPU, torch is installed from the index matching your driver (see [GPU & CUDA](/guide/gpu-cuda)).

<<< @/snippets/setup.txt{text}

<Badge type="tip" text="1.3" /> `pypilot setup --dry-run` prints the same steps as a plan, with each exact command line, the files it writes and why, and runs none of them. See [Dry runs](./dry-run).

## install

Install one package and record it in the manifest.

```bash
pypilot install <package> [--dry-run] [--json] [--path <dir>]
```

| Mode | Command run |
|---|---|
| uv project | `uv add <package>` |
| uv-pip | `uv pip install <package>` |
| pip | `pip install <package>`, then `pip freeze > requirements.txt` |
| pixi project | `pixi add --pypi <package>` |

<<< @/snippets/install-pixi-pkg.txt{text}

## fix python

Recompute the target Python and rebuild the environment on it, reinstalling the dependencies. Fails with a reason when there are no dependencies to solve for, or when they conflict. In a pixi project the move happens through pixi (`pixi add python=X.Y`).

```bash
pypilot fix python [--dry-run] [--json] [--path <dir>]
```

`pypilot fix` with no target means `fix python`.

## fix cuda

Re-pin `torch` and `tensorflow` to the build that matches your NVIDIA driver. In a uv project the PyTorch index is written to `pyproject.toml` (an explicit `[[tool.uv.index]]` plus `[tool.uv.sources]` for the torch family) and synced, so the next `uv sync` does not swap the CUDA build back. pixi projects are never re-pinned here.

```bash
pypilot fix cuda [--dry-run] [--json] [--path <dir>]
```

Without torch or tensorflow in the project:

```text
  [INFO ] no framework packages (torch, tensorflow) found in project.
```

## update-data

Refresh the bundled data tables now, ignoring `data_refresh_days`. Global: takes no `--path`.

```bash
pypilot update-data [--check] [--json]
```

Each table is reported as `ok` or `failed`, and a failure never breaks anything: the engine keeps the snapshot it was built with. This run happened before `conda_forge_packages.json` was published on `main`:

```text
Refreshing PyPilot's bundled data...
  ok    nvidia.json
  ok    frameworks.json
  ok    import_map.json
  ok    cuda_extensions.json
  failed conda_forge_packages.json: HTTP status client error (404 Not Found) for url (https://raw.githubusercontent.com/pypilot-dev/pypilot/main/helper/data/conda_forge_packages.json)

One or more datasets could not be refreshed; PyPilot keeps using the bundled snapshot for those.
```

When every table refreshes, the last line reads `All datasets refreshed.`

### update-data --check <Badge type="tip" text="1.3" /> {#update-data-check}

Fetch the tables from `main` and compare them with the copies in use, writing nothing. For each table: its schema, whether the bundled or a refreshed copy is in use and how old it is, and how it compares with `main`.

<<< @/snippets/update-data-check.txt{text}

| Exit | Meaning |
|---|---|
| `0` | The tables in use match `main` |
| `1` | Stale: `pypilot update-data` refreshes them |
| `2` | A fetch failed (offline, a proxy); the line for each table says why |

`pypilot doctor` shows the same state without any network access, as its `data tables` line (`refreshed 3 days ago (next in 4 days)`).

## migrate-conda

Translate `environment.yml` into a `pyproject.toml` (default) or a `pixi.toml`.

```bash
pypilot migrate-conda [--to pyproject|pixi] [--write] [--install] [--platform <p>]...
```

| Flag | Meaning |
|---|---|
| `--to pyproject` | Default. Writes `pyproject.toml` with the conda pins converted (`numpy=1.26` becomes `numpy==1.26.*`). Never overwrites an existing `pyproject.toml`, never deletes `environment.yml`. |
| `--to pixi` <Badge type="tip" text="1.3" /> | Preview the `pixi.toml` it would write: channels, conda packages, and the `pip:` section as `[pypi-dependencies]`. |
| `--write` | With `--to pixi`: write it. |
| `--install` | With `--write`: then run `pixi install`. |
| `--platform <p>` | With `--to pixi`: add a target platform (repeatable). |

```text
Wrote C:\src\conda\pyproject.toml

4 dependencies carried over as-is from environment.yml.
Conda package names do not always match their PyPI name, and some have no PyPI equivalent at all, so review the dependencies list before running `pypilot setup`.

The environment.yml was left in place; delete it once you're satisfied.
```

<<< @/snippets/migrate-conda-pixi.txt{text}

See [Conda projects](/guide/conda) and [Migrating to pixi](/pixi/migrate).

## migrate-to-pixi <Badge type="tip" text="1.3" /> {#migrate-to-pixi}

Preview the `pixi.toml` PyPilot would write for a pip or uv project (from `requirements.txt` and `[project.dependencies]`). Writes nothing unless asked.

```bash
pypilot migrate-to-pixi [--write] [--install] [--platform <p>]... [--path <dir>]
```

| Flag | Meaning |
|---|---|
| `--write` | Create `pixi.toml`. Existing manifests are never edited or deleted. |
| `--install` | With `--write`: then run `pixi install`. Without `--write` it is an error, because a dry run has nothing to install. |
| `--platform <p>`, `--platform=<p>` | Add a target platform besides the current one, such as `linux-64` (repeatable). |

<TerminalDemo src="migrate-geo" cmd="pypilot migrate-to-pixi --platform linux-64" title="~/src/geo" />

## install-pixi <Badge type="tip" text="1.3" /> {#install-pixi}

Install pixi with its official installer (`https://pixi.sh/install.ps1` through PowerShell on Windows, `https://pixi.sh/install.sh` elsewhere). If pixi is already installed it says so and does nothing.

```bash
pypilot install-pixi
```

<<< @/snippets/install-pixi.txt{text}

## logs <Badge type="tip" text="1.3" /> {#logs}

Print where the local log is, or its last entries. Every CLI command and every LSP action appends one JSON line; nothing is sent anywhere.

```bash
pypilot logs [--tail [N]] [--json]
```

| Flag | Meaning |
|---|---|
| (none) | Print the log file's path |
| `--tail [N]`, `--tail=N` | The last `N` entries (default 10), one summary line each; a failed entry lists its last three failed steps and the error |
| `--json` | With `--tail`: the raw JSON lines instead of the summary |

See [Logs, version & completions](./logs) for the file's location, rotation, fields and redaction.

## completions <Badge type="tip" text="1.3" /> {#completions}

Print a completion script for `bash`, `zsh`, `fish` or `powershell`. An unknown shell exits with status 1.

```bash
pypilot completions <bash|zsh|fish|powershell>
```

Loading them is shown in [Shell completion](./logs#shell-completion).

## version <Badge type="tip" text="1.3" /> {#version}

`pypilot --version` (and `-V`) prints one line, `pypilot 1.3.0`, because the editor extensions and `pypilot-cli` parse exactly that. `pypilot version`, or `pypilot --version --verbose`, adds the build details:

<<< @/snippets/version.txt{text}

`commit`, `target` and the date come from the build (each is missing when the build did not record it). The `data` lines are the same refresh state as `update-data --check`, read offline. `pypilot version --json` (or `--version --json`) prints all of it; see [Machine-readable output](./output#version-json).

## lsp

Run as a language server over stdio. Zed and VS Code start it for you; other editors can too, see [Other editors](/reference/editors) and the [LSP protocol](/reference/lsp).

```bash
pypilot lsp
```

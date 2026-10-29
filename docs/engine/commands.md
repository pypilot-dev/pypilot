# Command reference

Every mode and flag of the engine on `main`, which still reports version 1.2.2. Items marked <Badge type="tip" text="next" /> ship in the next release; release 1.2.2 has the rest. Output below was captured by running the engine on Windows x64 with an NVIDIA RTX A5000 laptop GPU and uv 0.13.5; project paths are shortened to `C:\src\…`. On your machine the versions PyPI reports will differ.

```text
pypilot <MODE> [ARG] [--path <dir>]
```

## Global options

| Option | Applies to | Meaning |
|---|---|---|
| `--path <dir>`, `-p <dir>`, `--path=<dir>` | all project modes | Project folder to analyze. Default: the current directory. If the folder's only Python code lives in one subfolder, that subfolder is used. |
| `--pixi-env <name>`, `--pixi-env=<name>` <Badge type="tip" text="next" /> | all project modes | Which pixi environment counts as the project's, for this run only. Default: the [`pixi_environment`](/reference/configuration#pixi-environment) setting (`default`). |
| `--json` <Badge type="tip" text="next" /> | every mode except `lsp` | Print one JSON document on stdout instead of text. See [Machine-readable output](./output). |
| `--markdown` <Badge type="tip" text="next" /> | `doctor`, `check` | Print the Markdown report the editors show under **Show details**. |
| `--version`, `-V`, `version` | — | Print `pypilot 1.2.2`. |
| `--help`, `-h`, `help` | — | Print usage. |

With no mode at all, `pypilot` runs `doctor`. An unknown mode prints the usage and exits with status 1.

**Exit status.** `0` when the command ran, `1` when it could not (bad arguments, a package not on PyPI, a failed install). `doctor` exits `0` even when it reports problems: read the findings, or use the [JSON output](./output) for scripting.

## doctor

Read-only report on the environment and the project's compatibility. Nothing is installed or written, except a small request file in the cache directory that tells a running language server to refresh its notification.

```bash
pypilot doctor [--path <dir>] [--pixi-env <name>]
```

Sections: **Environment** (uv, the project environment, pixi, interpreters found), **Project** (files, `requires-python`, dependencies), **pixi** (in pixi projects: platforms, lock status, environments), **Compatibility** (the supported range, the recommendation, each dependency's range) and **Findings**.

<TerminalDemo src="doctor" cmd="pypilot doctor" title="~/src/demo" />

When no Python satisfies every dependency:

<<< @/snippets/doctor-conflict.txt{text}

The [Findings reference](/reference/checks) lists everything doctor can report.

`pypilot doctor --json` and `pypilot doctor --markdown` ship in the next release; see [Machine-readable output](./output).

## check

Can one package run on this project's Python? Read-only. Takes a name or any requirement a requirements file accepts; a specifier is judged on the release it selects.

```bash
pypilot check <package> [--suggest-alternatives] [--path <dir>]
```

| Flag | Meaning |
|---|---|
| `--suggest-alternatives` <Badge type="tip" text="next" /> | Show the conda-forge alternative section even when the verdict passes. On a failure it is always shown. |

<<< @/snippets/check.txt{text}

<Badge type="tip" text="next" /> When the verdict is a failure (no files for this platform, no wheel for this Python, a source build), check says why and names the conda-forge build pixi could install instead:

<<< @/snippets/check-gdal.txt{text}

A name that is not on PyPI exits with status 1: ``pypilot check: `nosuchpkg-zzqq` is not on PyPI; check the spelling``.

## explain <Badge type="tip" text="next" /> {#explain}

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
  "pypilot": "1.2.2",
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
pypilot setup [--path <dir>] [--pixi-env <name>]
```

What it does, by mode:

| Mode | Steps |
|---|---|
| uv (`auto` with uv on `PATH`, or `uv`) | `uv init --bare` if there is no `pyproject.toml` (with `requires-python` and `.python-version` set to the chosen Python); `uv python install X.Y` if needed; `uv venv`; `uv add -r requirements.txt` when a requirements file exists, so its dependencies move into `pyproject.toml`. The chosen Python is pinned so a later `uv sync` keeps it. |
| `uv-pip` | uv's pip interface on `requirements.txt`: `uv venv` + `uv pip install`; no `pyproject.toml` is created. |
| `pip` (`auto` without uv, or `pip`) | `python -m venv` + `pip install`, then `pip freeze > requirements.txt`. It never downloads an interpreter: if the right Python is missing, it tells you which one to install. |
| pixi project | `pixi install`. No `.venv`, no uv, `pixi.lock` is pixi's. |
| `pixi` on a project without a pixi manifest <Badge type="tip" text="next" /> | Writes `pixi.toml` from the dependencies (as `migrate-to-pixi --write`), then `pixi install`. |

In projects with notebooks, setup also makes sure the environment has `ipykernel>=6.31.0,<7` (see [Notebooks](/guide/notebooks)). With an NVIDIA GPU, torch is installed from the index matching your driver (see [GPU & CUDA](/guide/gpu-cuda)).

<<< @/snippets/setup.txt{text}

::: info Still to come
`setup --dry-run`, `fix --dry-run` and `install --dry-run` print the plan (the commands and file changes) without running it.
:::

## install

Install one package and record it in the manifest.

```bash
pypilot install <package> [--path <dir>]
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
pypilot fix python [--path <dir>]
```

`pypilot fix` with no target means `fix python`.

## fix cuda

Re-pin `torch` and `tensorflow` to the build that matches your NVIDIA driver. In a uv project the PyTorch index is written to `pyproject.toml` (an explicit `[[tool.uv.index]]` plus `[tool.uv.sources]` for the torch family) and synced, so the next `uv sync` does not swap the CUDA build back. pixi projects are never re-pinned here.

```bash
pypilot fix cuda [--path <dir>]
```

Without torch or tensorflow in the project:

```text
  [INFO ] no framework packages (torch, tensorflow) found in project.
```

## update-data

Refresh the bundled data tables now, ignoring `data_refresh_days`. Global: takes no `--path`.

```bash
pypilot update-data
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

::: info Still to come
`update-data --check` reports whether the tables are stale without downloading them.
:::

## migrate-conda

Translate `environment.yml` into a `pyproject.toml` (default) or a `pixi.toml`.

```bash
pypilot migrate-conda [--to pyproject|pixi] [--write] [--install] [--platform <p>]...
```

| Flag | Meaning |
|---|---|
| `--to pyproject` | Default. Writes `pyproject.toml` with the conda pins converted (`numpy=1.26` becomes `numpy==1.26.*`). Never overwrites an existing `pyproject.toml`, never deletes `environment.yml`. |
| `--to pixi` <Badge type="tip" text="next" /> | Preview the `pixi.toml` it would write: channels, conda packages, and the `pip:` section as `[pypi-dependencies]`. |
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

## migrate-to-pixi <Badge type="tip" text="next" /> {#migrate-to-pixi}

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

## install-pixi <Badge type="tip" text="next" /> {#install-pixi}

Install pixi with its official installer (`https://pixi.sh/install.ps1` through PowerShell on Windows, `https://pixi.sh/install.sh` elsewhere). If pixi is already installed it says so and does nothing.

```bash
pypilot install-pixi
```

<<< @/snippets/install-pixi.txt{text}

## lsp

Run as a language server over stdio. Zed and VS Code start it for you; other editors can too, see [Other editors](/reference/editors) and the [LSP protocol](/reference/lsp).

```bash
pypilot lsp
```

## Still to come

Being built in parallel; documented here so the reference is complete once they land. <!-- verify after merge -->

| Addition | What it does |
|---|---|
| `--dry-run` on `setup`, `fix`, `install`; `doctor --fix --dry-run` | Print the plan (commands and file changes) without running it. |
| `pypilot logs [--tail [N]]` | Print the path of the local log (`<cache>/logs/pypilot.log`, one JSON line per CLI command and LSP action, rotated at 1 MB, secrets redacted), or its last N entries. |
| `pypilot completions <shell>` | Print a shell completion script. |
| `update-data --check` | Report stale tables without downloading. |
| `--version` details | Build details beyond the version number. |

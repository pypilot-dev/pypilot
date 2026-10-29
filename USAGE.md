# Using PyPilot

This walks through what actually happens, scenario by scenario, from the
moment you open a Python project in Zed with PyPilot installed. Everything
here maps to real code paths (`helper/src/core/solver.rs`, `helper/src/lsp/`,
`helper/src/matrix/`), not aspirational behavior.

## What triggers a scan

Opening a workspace that contains Python files starts the `pypilot lsp`
language server. If `auto_check_on_open = true` (the default), it runs a
full assessment once: probe the environment (venv, uv, installed
interpreters), parse the project's dependency files, fetch PyPI metadata for
each dependency, and compute which Python versions the whole set supports.

The same assessment also runs whenever you trigger it manually, from a Zed
task (see Commands below) or by editing a dependency file and saving it.

## Walkthrough: cloning a repo with `requirements.txt`

Say the repo has `requirements.txt` with `mediapipe`, `numpy`, and no
`.venv` yet.

1. PyPilot reads `requirements.txt`, including any version pins
   (`mediapipe==0.10.14` is judged on that release, not the newest one).
2. For each package it fetches wheel tags and `requires_python` from PyPI,
   then intersects the supported Python versions across all of them.
3. If the intersection is non-empty, it picks the newest version in range
   and shows a notification: **"No environment set up yet"**, with the
   reason (e.g. "Dependencies support Python 3.9-3.12. Recommended: 3.12
   (upper bound set by mediapipe, which supports 3.9-3.12)"), and a **"Fix
   everything"** button.
4. Clicking it runs the same logic as `pypilot setup`: install the target
   Python if missing (uv mode only), create the venv, install the
   dependencies. Pip mode never installs a missing interpreter; it tells you
   which one you need instead.

If the intersection is empty, two dependencies want incompatible Python
ranges. PyPilot shows an **error**, names both packages and their supported
ranges, and does not offer an auto-fix button, since there's no single
version to recreate the environment on. You have to resolve the conflict
yourself (pin one package differently, drop one, etc.).

## Every finding PyPilot can surface

| Situation | Severity | What you see | Auto-fix? |
|---|---|---|---|
| No venv yet, versions agree | Warning | "No environment set up yet" + recommended version | Yes — "Fix environment" |
| Venv exists, wrong Python | Error | "Python X.Y is not supported by this project" + which package(s) block it | Yes — "Recreate with Python X.Y" |
| Venv exists, right Python | — | Nothing. Silent on purpose. | — |
| Dependencies conflict (empty intersection) | Error | Both blocking packages named, each one's supported range | No — manual |
| A dependency name doesn't resolve on PyPI | Warning | "Could not resolve `name`" (typo or private package) | No — manual |
| A dependency has no wheels, will compile from source | Warning | Which package, that it'll build from an sdist | No — manual |
| `environment.yml` found (conda project) | Info | Points at `pypilot migrate-conda` (`--to pixi` for a pixi.toml) | Command, not a button |
| `uv --dry-run` disagrees with the metadata verdict | Warning | Names the real resolver conflict metadata alone can't see (yanked release, transitive pin) | No — manual |
| GPU present, torch/tensorflow declared | Info | Which CUDA build was picked for your driver | Applied automatically on install |
| No GPU | — | Silent; installs the plain CPU build | — |
| Apple Silicon, torch/tensorflow declared | Info | Note that you get the MPS backend, not CUDA | — |

## Two separate systems: onboarding scan vs. live diagnostics

The scan above (F5) runs once per relevant event and shows a **notification**
in Zed's toast/message area. It's about the environment as a whole: which
Python, is the venv right, do the pins conflict.

Separately, while you're actually editing a file, PyPilot runs live
diagnostics (F4) on your imports:

- Type `import mediapipe` in a project whose venv is on an unsupported
  Python version, and the import gets a squiggle. `ctrl-.` (code actions)
  offers to recreate the environment on the version that supports it.
- If the package is already installed, PyPilot also checks it structurally:
  it reads what the installed package on disk actually exports. Type
  `mp.solutions` where `solutions` was removed in the installed release
  (this is real: mediapipe 0.10.35 dropped it), and you get a squiggle
  naming the missing attribute and what the package does provide, before you
  ever run the file. This only fires when PyPilot can be sure the attribute
  is genuinely absent — if the module uses dynamic attribute resolution
  (`__getattr__`), it stays quiet rather than guessing.

Live diagnostics don't wait for a manual scan or a saved file; they update
as you type, the same as any other LSP diagnostic.

## If you miss the notification (or dismiss it)

Nothing is lost. The assessment isn't a one-shot popup, it's re-derivable
any time:

- Run **`pypilot: show details (doctor report)`** from `task: spawn` for a
  full read-only report — same information the toast would have shown,
  without needing the toast to still be on screen.
- Run **`pypilot: fix environment (set up environment)`** to apply whatever
  the current assessment recommends, whether or not you saw the original
  notification.
- The diagnostics/squiggles on your buffers aren't a notification at all;
  they persist in the Problems panel and on the line itself until the
  underlying issue is fixed, so there's nothing to "miss" there.

## Notification volume: `notifications` setting

- `"problems-only"` (default): a toast for warnings and errors only. A
  correctly-configured project stays silent.
- `"all"`: also raises informational findings, like which CUDA build got
  picked, or that a conda project was detected.
- `"off"`: no toasts at all. Diagnostics and code actions on your buffers
  still work; only the notification is suppressed.

## GPU / CUDA scenario in detail

If the project declares `torch` or `tensorflow` and PyPilot detects an
NVIDIA GPU (`nvidia-smi`), it reads the driver version, maps it to the
newest CUDA runtime that driver supports, and picks the matching build
(`cu121`, `cu124`, etc.) so the install pulls the right wheel instead of
whatever the default index would hand back. This applies automatically on
`pypilot setup` / `pypilot install` / the "Fix environment" button; you don't
choose the build yourself. If your driver is too old for any build the
declared framework version ships, that's surfaced as a finding naming the
gap. Apple Silicon gets a note that you're on the MPS backend, not CUDA. No
GPU at all means the plain CPU wheel, silently, no finding raised.

## Conda scenario in detail

If the workspace has `environment.yml` and no `pyproject.toml`, PyPilot
doesn't try to manage conda directly (translating conda package names to
PyPI ones isn't reliable enough to automate). It raises an Info-level
finding pointing at `pypilot migrate-conda`, which generates a
`pyproject.toml` from the conda file's dependencies (version pins
preserved, including conda's `=` prefix-match semantics converted to
`==X.Y.*`). It never deletes `environment.yml` and never overwrites an
existing `pyproject.toml`. Review the generated file before running
`pypilot setup` on it: conda package names don't always match their PyPI
name, and some have no PyPI equivalent at all.

## Commands (via Zed tasks, `task: spawn`)

Zed doesn't let extensions add command palette entries, so every action
ships as a task. Copy `tasks/pypilot.json` into `.zed/tasks.json` or your
global Zed `tasks.json` first.

| Task | Does |
|---|---|
| `pypilot: fix environment (set up environment)` | Build/rebuild the environment on the recommended Python |
| `pypilot: show details (doctor report)` | Read-only report, changes nothing |
| `pypilot: fix python version` | Rebuild the environment on the right version specifically |
| `pypilot: fix cuda (re-pin torch/tensorflow to the driver)` | Reinstall those two packages against the matched CUDA build |
| `pypilot: update data (refresh bundled tables)` | Force-refresh driver/framework/import tables from this repo |
| `pypilot: migrate conda to pyproject.toml` | One-shot conda → pyproject.toml translation |
| `pypilot: preview migration to pixi (pixi.toml)` | Shows the pixi.toml `migrate-to-pixi` would write; changes nothing |
| `pypilot: migrate to pixi (write pixi.toml and install)` | Writes pixi.toml next to your untouched manifests, then `pixi install` |
| `pypilot: install pixi` | Runs pixi's official installer |

The toast's "Fix environment" button and the task of the same name call the
identical function, so they can't drift apart. From a terminal,
`pypilot doctor --fix` prints the report and then runs that same fix.

## Previewing changes: `--dry-run`

`setup`, `install <package>`, `fix python`, `fix cuda` and `doctor --fix`
take `--dry-run`: they print every step they would run, in order, with its
exact command line, the files it would create or change, and why, under the
solver's reason for the chosen Python. Nothing is executed, written or
downloaded; only the read-only probes and PyPI lookups the analysis always
does run. The plan comes from the command's own code path with each change
recorded instead of made, so it is what the real run does. When the real
run would stop (pip mode without the needed Python, say), the plan says
where and exits non-zero.

```bash
pypilot setup --dry-run
pypilot fix python --dry-run --json
```

## Machine-readable output: `--json` and `--markdown`

Every CLI mode except `lsp` takes `--json` and then prints exactly one JSON
document on stdout; progress lines go to stderr. Exit codes are the same as
without it. `doctor` and `check` also take `--markdown`, which prints the
Markdown report the editor shows under "Show details", ready to paste into an
issue.

```bash
pypilot doctor --json > report.json
pypilot check mediapipe==0.10.14 --json
pypilot doctor --markdown | clip
```

### JSON output

Every document starts with the same three keys, then the command's own:

```json
{ "pypilot": "1.3.0", "command": "check", "ok": true, ... }
```

| Key | Meaning |
|---|---|
| `pypilot` | engine version that wrote it |
| `command` | `doctor`, `check`, `setup`, `install`, `fix python`, `fix cuda`, `update-data`, `migrate-conda`, `migrate-to-pixi`, `install-pixi`, `version` |
| `ok` | the command's own verdict (see each command); a failed command also exits non-zero, except `doctor`, `check` and `update-data`, which only report (`update-data --check` exits 1 when stale, 2 when a fetch failed) |
| `error` | only when the command failed before it had a report: `{ "pypilot", "command", "ok": false, "error": "..." }` |

Field names are stable: new fields may be added, existing ones are never
renamed, removed or retyped. Versions are strings (`"3.12"`); a set of
Pythons is a list of them, oldest first (`["3.10", "3.11"]`). Paths are
strings. A command's fields are always present, `null` when there is no
value (the variant-specific keys of `lock` aside).

Shared shapes:

- **finding**: `{ "severity": "error"|"warning"|"info", "title", "detail", "fix": { "kind": ..., "python"? } }`.
  `fix.kind` is one of `setup_environment`, `recreate_with_python` (with
  `python`), `migrate_conda`, `migrate_to_pixi`, `manual`.
- **package analysis**: `{ "name", "version", "supported": [pythons], "has_platform_wheels", "sdist_only", "has_foreign_wheels", "no_files_for_platform", "wheel_platforms": [...], "requires_python" }`.
- **conflict**: `{ "low": analysis, "high": analysis, "explanation" }`; `low`
  is the package with the lowest newest Python, `high` the one with the
  highest oldest Python.
- **summary** (what `setup`, `install` and `fix python` ran): `{ "package_manager", "resolved_mode": "uv_full"|"uv_pip_compat"|"pure_pip"|"pixi", "python", "venv_path", "why", "steps": [{ "name", "ok", "detail" }], "ok" }`.

Per command:

| Command | `ok` | Fields |
|---|---|---|
| `doctor` | no finding has severity `error` | `workspace`, `package_manager`, `environment` (`uv`: `{path, version, managed}`, `venv`: `{path, python, broken, kind: {type: "venv"} \| {type: "pixi", name}}`, `interpreters`: `[{command, path, version}]`), `project` (`sources`, `requires_python`, `pixi_python`, `dependencies`, `conda_dependencies`: `[{name, spec}]`, `has_conda_env`), `pixi` (`null`, or `{manifest, platforms, lock: {status: "missing"\|"up_to_date"\|"stale"\|"unsolvable"\|"unknown", changes?, reason?}, environments: [{name, features, selected, installed, python, python_spec, conda, pypi}]}`), `compatibility` (`null` without dependencies, else `{platform, supported, supported_range, recommended, packages: [analysis], unresolved: [{name, error}], unreachable: [{name, error}], conflicts: [conflict]}`), `target_python`, `findings`, `data_tables` (the text report's one-line refresh state) |
| `check` | the package installs and runs here: not `no_files`, `conflict`, `unsupported`, nor a source build | `requirement`, `analysis`, `supported_range`, `platform`, `installed`, `environment_python`, `pixi_project`, `verdict` (`{kind: "supported"\|"unsupported"\|"no_environment"\|"no_files"\|"conflict", status: "ok"\|"error"\|"info", message}`), `conflict` (the project dependency it shares no Python with, or `null`), `why` (reasons it fails here), `alternative` (conda-forge build, or `null`) |
| `setup` | `summary.ok` | `workspace`, `summary`, `pixi_advice` (a failed build's conda-forge hint, or `null`) |
| `install` | `summary.ok` | `workspace`, `package`, `summary`, `pixi_advice` |
| `fix python` | `summary.ok` | `workspace`, `target_python`, `summary` (`null` with a `message` when already on it), `pixi_advice` |
| `fix cuda` | every framework re-pinned | `message` (why nothing was tried, or `null`), `frameworks`: `[{package, finding, spec, index_url, action: "pinned_in_pyproject"\|"installed"\|"skipped", ok, detail}]`, `error` |
| `update-data` | every dataset refreshed | `datasets`: `[{name, ok, error}]` |
| `update-data --check` | the tables in use match `main` | `check`: `true`, `status`: `"current"\|"stale"\|"error"`, `refresh_days`, `summary`, `cache_dir`, `tables`: `[{name, bundled_schema, cache_age_days, cache_schema, in_use: "bundled"\|"cache", main: "same"\|"differs"\|"missing"\|"error", main_schema, main_matches_bundled, error}]` |
| `version` (`--version --json`) | always | `version`, `commit`, `commit_date`, `target` (each `null` when not recorded at build time), `data`: `{bundled_date, refresh_days, summary, tables: [{name, bundled_schema, cache_age_days, cache_schema, in_use}]}` |
| `migrate-conda` | written | `written`, `dependencies`, `environment_file`; with `--to pixi`, as `migrate-to-pixi` |
| `migrate-to-pixi` | the requested `pixi install` succeeded (always for a preview) | `summary`: `{manifest_path, manifest, preview, written, sources, python, platforms, channels, deps: [{name, value, kind: "conda"\|"pypi", from, reason}], skipped: [{line, reason}], notes, install: summary \| null, next_command}` |
| `install-pixi` | installed | `path`, `version`, `already_installed`, `refresh_path` |

With `--dry-run`, `setup`, `install`, `fix python`, `fix cuda` and
`doctor --fix` print a plan instead (`command` is `doctor --fix` for the
last): `{ "pypilot", "command", "ok", "dry_run": true, "plan": { "mode", "python", "why", "steps": [{ "name", "command": [argv], "writes": [paths], "why" }], "notes": [{ "name", "ok", "detail" }], "blocked" } }`.
`mode` is a `resolved_mode` value; `command` is the exact argument vector,
program first, and empty for a step that is not a process (writing the
PyTorch index into pyproject.toml, downloading uv, the GPU self-test);
`blocked` is why the real run would stop, else `null`, and `ok` is
`blocked == null`.

`explain --json` predates this and keeps its own top level:
`{ "pypilot", "kind": "package"|"project", "explanation": {...} }`.

## Logs, version, completions

**Local log.** Every CLI command and every LSP command or workspace scan
appends one JSON line to `pypilot.log` in the platform cache dir
(`%LOCALAPPDATA%\PyPilot\pypilot\cache\logs` on Windows,
`~/Library/Caches/dev.PyPilot.pypilot/logs` on macOS,
`~/.cache/pypilot/logs` on Linux). It rotates at 1 MB and keeps three files.
Nothing is sent anywhere.

```bash
pypilot logs              # the log's path
pypilot logs --tail 5     # the last 5 entries, summarized
pypilot logs --tail --json  # the last 10 entries as raw JSON lines
```

Each line holds `ts` (UTC), `pypilot` (engine version), `source` (`cli` or
`lsp`), `command`, `args`, `workspace`, `mode` (the `package_manager`
setting), `resolved_mode`, `ok`, `duration_ms`, `steps` (every subprocess:
`{cmd, args, ok, exit, duration_ms, stderr}`, with the last 2000 characters
of stderr only for failures) and `error`. Credentials in URLs, `NAME=value`
pairs whose name looks secret and well-known token formats are masked; the
environment is never recorded. `PYPILOT_NO_LOG=1` turns the log off and
`PYPILOT_LOG_DIR` moves it.

**Version.** `pypilot --version` prints only `pypilot X.Y.Z` (wrappers parse
it). `pypilot version` (or `--version --verbose`) adds the git commit, the
target triple, and each data table's schema and whether the bundled copy or
a refreshed one is in use; `--json` prints all of it.

**Data freshness.** `pypilot update-data --check` fetches the tables from
`main` and compares them with the copies in use, writing nothing. Exit `0`
current, `1` stale (`pypilot update-data` refreshes them), `2` a fetch
failed. `pypilot doctor` shows the same state offline as `data tables :
refreshed 3 days ago (next in 4 days)`.

**Shell completion.**

```bash
source <(pypilot completions bash)                                  # ~/.bashrc
source <(pypilot completions zsh)                                   # ~/.zshrc
pypilot completions fish > ~/.config/fish/completions/pypilot.fish
pypilot completions powershell | Out-String | Invoke-Expression     # $PROFILE
```

## Settings

`.zed/pypilot.toml` or `pypilot.toml` in the project root, or the global
config directory for defaults across all projects:

```toml
package_manager    = "uv"             # "auto", "uv", "uv-pip", "pip" or "pixi"
notifications      = "problems-only"  # "all", "problems-only", or "off"
auto_check_on_open = true
data_refresh_days  = 7                # 0 stays fully offline
```

`package_manager = "pip"` runs the identical compatibility checks; the only
difference is pip can't fetch a missing interpreter for you, so PyPilot
tells you which Python you need instead of installing it.

`package_manager = "pixi"` manages the project with pixi. pixi projects are
detected and managed with pixi whatever the setting says; on a project
without a pixi manifest, setup writes a `pixi.toml` from its dependencies
first (see `pypilot migrate-to-pixi`), then runs `pixi install`. Without
pixi installed it changes nothing and offers `pypilot install-pixi`.

`data_refresh_days` controls how often the bundled driver/framework/import
tables refresh from this repo in the background. They ship inside the
binary, so an offline machine is never wrong, only potentially stale. `0`
disables the network check entirely; `pypilot update-data` forces a refresh
on demand regardless of the TTL.

# Machine-readable output <Badge type="tip" text="next" />

On `main`, every CLI mode except `lsp` and `completions` takes `--json`, and `doctor` and `check` take `--markdown` (alias `--md`). `--format=json` and `--format=markdown` work too; given both, JSON wins. All of it ships in the next engine release; release 1.2.2 prints text only.

```bash
pypilot doctor --json > report.json
pypilot check mediapipe==0.10.14 --json
pypilot doctor --markdown            # paste into an issue
```

With `--json` the command prints exactly one JSON document on stdout; progress lines go to stderr. Exit codes are the same as without it. (`pypilot logs --json` is the exception: it prints the raw log lines, one JSON object per line, without the envelope. See [Logs](./logs#fields).)

The human-readable text of the other modes is for people and may change between releases. Do not parse it; use `--json`.

## The envelope

Every document starts with the same three keys, then the command's own fields:

```json
{ "pypilot": "1.2.2", "command": "check", "ok": true }
```

| Key | Meaning |
|---|---|
| `pypilot` | Engine version that wrote it |
| `command` | `doctor`, `doctor --fix`, `check`, `setup`, `install`, `fix python`, `fix cuda`, `update-data`, `migrate-conda`, `migrate-to-pixi`, `install-pixi`, `version` |
| `ok` | The command's own verdict (see the table below). A failed command also exits non-zero, except `doctor`, `check` and `update-data`, which only report (`update-data --check` exits `1` when stale, `2` when a fetch failed) |
| `error` | Only when the command failed before it had a report: `{ "pypilot", "command", "ok": false, "error": "…" }` |

**Stability.** Field names are stable: new fields may be added, existing ones are never renamed, removed or retyped. Versions are strings (`"3.12"`); a set of Pythons is a list of them, oldest first. Paths are strings. A command's fields are always present, `null` when there is no value (the variant-specific keys of `pixi.lock` aside). Key order is not part of the contract: most documents keep the order shown here, `version` and `update-data --check` print theirs alphabetically.

## Per command

| Command | `ok` means | Fields |
|---|---|---|
| `doctor` | no finding has severity `error` | `workspace`, `package_manager`, `environment` (`uv`, `venv`, `interpreters`), `project` (`sources`, `requires_python`, `pixi_python`, `dependencies`, `conda_dependencies`, `has_conda_env`), `pixi` (`null`, or `manifest`, `platforms`, `lock`, `environments`), `compatibility` (`null` without dependencies, else `platform`, `supported`, `supported_range`, `recommended`, `packages`, `unresolved`, `unreachable`, `conflicts`), `target_python`, `findings`, `data_tables` (the text report's one-line refresh state) |
| `check` | the package installs and runs here | `requirement`, `analysis`, `supported_range`, `platform`, `installed`, `environment_python`, `pixi_project`, `verdict` (`kind`: `supported`, `unsupported`, `no_environment`, `no_files` or `conflict`; `status`: `ok`, `error` or `info`; `message`), `conflict` (the project dependency it shares no Python with, as a conflict pair, or `null`), `why` (reasons it fails here), `alternative` (the conda-forge build, or `null`) |
| `setup` | `summary.ok` | `workspace`, `summary`, `pixi_advice` |
| `install` | `summary.ok` | `workspace`, `package`, `summary`, `pixi_advice` |
| `fix python` | `summary.ok` | `workspace`, `target_python`, `summary` (`null` with a `message` when already on it), `pixi_advice` |
| `fix cuda` | every framework re-pinned | `message`, `frameworks` (`package`, `finding`, `spec`, `index_url`, `action`: `pinned_in_pyproject`, `installed` or `skipped`, `ok`, `detail`), `error` |
| `update-data` | every dataset refreshed | `datasets` (`name`, `ok`, `error`) |
| `update-data --check` | the tables in use match `main` | `check: true`, `status` (`current`, `stale` or `error`), `refresh_days`, `summary`, `cache_dir`, `tables`: `[{ name, bundled_schema, cache_age_days, cache_schema, in_use: "bundled" \| "cache", main: "same" \| "differs" \| "missing" \| "error", main_schema, main_matches_bundled, error }]` |
| `version` | always | `version`, `commit`, `commit_date`, `target` (each `null` when the build did not record it), `data`: `{ bundled_date, refresh_days, summary, tables: [{ name, bundled_schema, cache_age_days, cache_schema, in_use }] }` |
| `migrate-conda` | written | `written`, `dependencies`, `environment_file`; with `--to pixi`, as `migrate-to-pixi` |
| `migrate-to-pixi` | the requested `pixi install` succeeded (always, for a preview) | `summary`: `manifest_path`, `manifest`, `preview`, `written`, `sources`, `python`, `platforms`, `channels`, `deps` (`name`, `value`, `kind`: `conda` or `pypi`, `from`, `reason`), `skipped`, `notes`, `install`, `next_command` |
| `install-pixi` | installed | `path`, `version`, `already_installed`, `refresh_path` |

### Shared shapes

- **finding**: `{ "severity": "error" | "warning" | "info", "title", "detail", "fix": { "kind", "python"? } }`. `fix.kind` is `setup_environment`, `recreate_with_python` (with `python`), `migrate_conda`, `migrate_to_pixi` or `manual`.
- **package analysis**: `{ "name", "version", "supported", "has_platform_wheels", "sdist_only", "has_foreign_wheels", "no_files_for_platform", "wheel_platforms", "requires_python" }`.
- **conflict**: `{ "low": analysis, "high": analysis, "explanation" }`. `low` is the package with the lowest newest Python, `high` the one with the highest oldest Python.
- **summary** (what `setup`, `install` and `fix python` ran): `{ "package_manager", "resolved_mode": "uv_full" | "uv_pip_compat" | "pure_pip" | "pixi", "python", "venv_path", "why", "steps": [{ "name", "ok", "detail" }], "ok" }`.

## Dry runs {#dry-run}

With `--dry-run`, `setup`, `install`, `fix python`, `fix cuda` and `doctor --fix` print a plan instead of their usual document:

```json
{ "pypilot": "1.2.2", "command": "setup", "ok": true, "dry_run": true,
  "plan": { "mode": "uv_full", "python": "3.12", "why": "…",
            "steps": [{ "name", "command": [argv], "writes": [paths], "why" }],
            "notes": [{ "name", "ok", "detail" }], "blocked": null } }
```

`ok` is `blocked == null`. The fields and a full example are on [Dry runs](./dry-run#json).

## Examples

`pypilot doctor --json` in the project from the [home page](/):

<<< @/snippets/doctor-json.json{json}

`pypilot check mediapipe==0.10.14 --json`:

<<< @/snippets/check-json.json{json}

`pypilot doctor --markdown` prints the report the editors show under **Show details**:

<<< @/snippets/doctor-md.txt{md}

### `version --json` {#version-json}

`pypilot version --json`:

```json
{
  "command": "version",
  "commit": "035ad2f2045a",
  "commit_date": "2026-10-29",
  "data": {
    "bundled_date": "2026-10-29",
    "refresh_days": 7,
    "summary": "refreshed just now (next in 6 days); 1 of 5 bundled",
    "tables": [
      { "bundled_schema": 3, "cache_age_days": 0.0, "cache_schema": 3, "in_use": "cache", "name": "nvidia.json" },
      { "bundled_schema": 1, "cache_age_days": null, "cache_schema": null, "in_use": "bundled", "name": "conda_forge_packages.json" }
    ]
  },
  "ok": true,
  "pypilot": "1.2.2",
  "target": "x86_64-pc-windows-msvc",
  "version": "1.2.2"
}
```

(Three of the five tables trimmed.)

## `explain --json`

`explain` predates the envelope and keeps its own top level: `{ "pypilot", "kind": "package" | "project", "explanation": { … } }`. See the [example on the command reference](./commands#explain).

## In CI

Fail a job when the project has an error-level finding (no supported Python, a conflict, a Python the environment cannot run):

```bash
pip install pypilot-cli
pypilot doctor --json > pypilot.json
python -c "import json,sys; sys.exit(0 if json.load(open('pypilot.json'))['ok'] else 1)"
```

`doctor` itself always exits `0` when it ran, so the check reads `ok`. Note that `pypilot-cli` 1.2.2 pins engine 1.2.2, which has no `--json` yet; this needs the next release of both. With the next `pypilot-cli`, `PYPILOT_ENGINE_VERSION=vX.Y.Z` can also select a newer engine release (see [environment variables](/python/#environment-variables)).

From Python, `pypilot.run([...], capture_output=True).json()` parses the document for you; see the [Python API](/python/api).

To check that the data tables are current in the same job:

```bash
pypilot update-data --check --json > tables.json || echo "tables stale or unreachable (exit $?)"
```

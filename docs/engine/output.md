# Machine-readable output <Badge type="tip" text="next" />

On `main`, every CLI mode except `lsp` takes `--json`, and `doctor` and `check` take `--markdown`. Both ship in the next engine release; release 1.2.2 prints text only.

```bash
pypilot doctor --json > report.json
pypilot check mediapipe==0.10.14 --json
pypilot doctor --markdown            # paste into an issue
```

With `--json` the command prints exactly one JSON document on stdout; progress lines go to stderr. Exit codes are the same as without it.

The human-readable text of the other modes is for people and may change between releases. Do not parse it; use `--json`.

## The envelope

Every document starts with the same three keys, then the command's own fields:

```json
{ "pypilot": "1.2.2", "command": "check", "ok": true }
```

| Key | Meaning |
|---|---|
| `pypilot` | Engine version that wrote it |
| `command` | `doctor`, `check`, `setup`, `install`, `fix python`, `fix cuda`, `update-data`, `migrate-conda`, `migrate-to-pixi`, `install-pixi` |
| `ok` | The command's own verdict (see the table below). A failed command also exits non-zero, except `doctor`, `check` and `update-data`, which only report |
| `error` | Only when the command failed before it had a report: `{ "pypilot", "command", "ok": false, "error": "…" }` |

**Stability.** Field names are stable: new fields may be added, existing ones are never renamed, removed or retyped. Versions are strings (`"3.12"`); a set of Pythons is a list of them, oldest first. Paths are strings. A command's fields are always present, `null` when there is no value.

## Per command

| Command | `ok` means | Fields |
|---|---|---|
| `doctor` | no finding has severity `error` | `workspace`, `package_manager`, `environment` (`uv`, `venv`, `interpreters`), `project` (`sources`, `requires_python`, `pixi_python`, `dependencies`, `conda_dependencies`, `has_conda_env`), `pixi` (`null`, or `manifest`, `platforms`, `lock`, `environments`), `compatibility` (`null` without dependencies, else `platform`, `supported`, `supported_range`, `recommended`, `packages`, `unresolved`, `conflicts`), `target_python`, `findings` |
| `check` | the package installs and runs here | `requirement`, `analysis`, `supported_range`, `platform`, `installed`, `environment_python`, `pixi_project`, `verdict` (`kind`: `supported`, `unsupported`, `no_environment`, `no_files` or `conflict`; `status`; `message`), `conflict`, `why`, `alternative` |
| `setup` | `summary.ok` | `workspace`, `summary`, `pixi_advice` |
| `install` | `summary.ok` | `workspace`, `package`, `summary`, `pixi_advice` |
| `fix python` | `summary.ok` | `workspace`, `target_python`, `summary` (`null` with a `message` when already on it), `pixi_advice` |
| `fix cuda` | every framework re-pinned | `message`, `frameworks` (`package`, `finding`, `spec`, `index_url`, `action`: `pinned_in_pyproject`, `installed` or `skipped`, `ok`, `detail`), `error` |
| `update-data` | every dataset refreshed | `datasets` (`name`, `ok`, `error`) |
| `migrate-conda` | written | `written`, `dependencies`, `environment_file`; with `--to pixi`, as `migrate-to-pixi` |
| `migrate-to-pixi` | the requested `pixi install` succeeded (always, for a preview) | `summary`: `manifest_path`, `manifest`, `preview`, `written`, `sources`, `python`, `platforms`, `channels`, `deps` (`name`, `value`, `kind`: `conda` or `pypi`, `from`, `reason`), `skipped`, `notes`, `install`, `next_command` |
| `install-pixi` | installed | `path`, `version`, `already_installed`, `refresh_path` |

### Shared shapes

- **finding**: `{ "severity": "error" | "warning" | "info", "title", "detail", "fix": { "kind", "python"? } }`. `fix.kind` is `setup_environment`, `recreate_with_python` (with `python`), `migrate_conda`, `migrate_to_pixi` or `manual`.
- **package analysis**: `{ "name", "version", "supported", "has_platform_wheels", "sdist_only", "has_foreign_wheels", "no_files_for_platform", "wheel_platforms", "requires_python" }`.
- **conflict**: `{ "low": analysis, "high": analysis, "explanation" }`. `low` is the package with the lowest newest Python, `high` the one with the highest oldest Python.
- **summary** (what `setup`, `install` and `fix python` ran): `{ "package_manager", "resolved_mode": "uv_full" | "uv_pip_compat" | "pure_pip" | "pixi", "python", "venv_path", "why", "steps": [{ "name", "ok", "detail" }], "ok" }`.

## Examples

`pypilot doctor --json` in the project from the [home page](/):

<<< @/snippets/doctor-json.json{json}

`pypilot check mediapipe==0.10.14 --json`:

<<< @/snippets/check-json.json{json}

`pypilot doctor --markdown` prints the report the editors show under **Show details**:

<<< @/snippets/doctor-md.txt{md}

## `explain --json`

`explain` predates the envelope and keeps its own top level: `{ "pypilot", "kind": "package" | "project", "explanation": { … } }`. See the [example on the command reference](./commands#explain).

## In CI

Fail a job when the project has an error-level finding (no supported Python, a conflict, a Python the environment cannot run):

```bash
pip install pypilot-cli
pypilot doctor --json > pypilot.json
python -c "import json,sys; sys.exit(0 if json.load(open('pypilot.json'))['ok'] else 1)"
```

`doctor` itself always exits `0` when it ran, so the check reads `ok`. Note that `pypilot-cli` 1.2.2 pins engine 1.2.2, which has no `--json` yet; use a newer pin, or put a newer engine on `PATH`.

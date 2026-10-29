# Migrating to pixi <Badge type="tip" text="next" />

Two commands, one for each starting point:

| From | Command |
|---|---|
| pip or uv: `requirements.txt`, `[project.dependencies]` | `pypilot migrate-to-pixi` |
| conda: `environment.yml` | `pypilot migrate-conda --to pixi` |

Both are **dry runs by default**: they print the `pixi.toml` they would write as a diff and change nothing.

## The rules

- A separate `pixi.toml` is written. `pyproject.toml`, `requirements.txt` and `environment.yml` are never edited or deleted, so CI and pip keep working. pixi prefers `pixi.toml` when both exist.
- Each PyPI dependency that is packaged on conda-forge (per prefix.dev's PyPI-to-conda mapping) **and** built for every target platform goes to `[dependencies]` under its conda name (`torch` becomes `pytorch`). The rest go to `[pypi-dependencies]`, with the reason as a comment.
- Version specifiers carry over only when conda can express them exactly; otherwise the dependency stays on PyPI.
- `python` is the version PyPilot recommends, within what the conda-forge builds support.
- Platforms default to the current one; add more with `--platform` (repeatable), for example `--platform linux-64 --platform osx-arm64`.
- Environment markers, editable installs and `-r` includes are listed in the file as comments to add by hand.
- Offline, everything stays on PyPI. The mapping is cached for `data_refresh_days`.

## Step by step: a pip project

**1. Preview.** In the project folder:

<TerminalDemo src="migrate-geo" cmd="pypilot migrate-to-pixi --platform linux-64" title="~/src/geo" />

Every dependency was found on conda-forge for both platforms, so all three are conda dependencies and GDAL comes with its native libraries.

For a dependency conda-forge does not have, the preview says why it stays on PyPI:

```text
[pypi-dependencies]
mediapipe = "==0.10.14"  # not on conda-forge
```

**2. Write it**, and optionally install in the same step:

```bash
pypilot migrate-to-pixi --write              # writes pixi.toml
pypilot migrate-to-pixi --write --install    # and runs pixi install
```

For a small project with `requests` and `rich` in `requirements.txt`, writing and installing takes a few seconds:

<<< @/snippets/migrate-install.txt{text}

`--install` without `--write` is an error, because a dry run has nothing to install.

**3. Check it.**

<<< @/snippets/doctor-pixi.txt{text}

From now on PyPilot treats the folder as a pixi project: see [PyPilot in pixi projects](./projects).

**4. Commit** `pixi.toml` and `pixi.lock`. Add `.pixi/` to `.gitignore`.

## Step by step: a conda project

```yaml
# environment.yml
name: geo
channels:
  - conda-forge
dependencies:
  - python=3.11
  - numpy=1.26
  - gdal
  - pip
  - pip:
      - rich>=13
```

<<< @/snippets/migrate-conda-pixi.txt{text}

Channels and conda packages are kept, conda pins become pixi specs (`numpy=1.26` → `1.26.*`), and the `pip:` section becomes `[pypi-dependencies]`. Then:

```bash
pypilot migrate-conda --to pixi --write --install
```

## From the editor

- **VS Code**: **PyPilot: Initialize pixi project** shows the preview and writes it only after you confirm, optionally running `pixi install`.
- **Zed**: the tasks `pypilot: preview migration to pixi (pixi.toml)` and `pypilot: migrate to pixi (write pixi.toml and install)`, or the **Initialize pixi project** button on the notification.
- Both: `package_manager = "pixi"` makes `setup` write `pixi.toml` (as `--write` would) and run `pixi install` on a project without a pixi manifest.

## Undo

Delete `pixi.toml`, `pixi.lock` and `.pixi/`. Nothing else was changed.

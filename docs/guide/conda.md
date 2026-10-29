# Conda projects

PyPilot does not manage conda environments: translating conda package names to PyPI names is not reliable enough to automate silently. When a project has an `environment.yml`, doctor raises an Info finding and points at a migration instead.

```text
  [INFO ] Conda environment detected
          environment.yml found. Run `pypilot migrate-conda` to generate a pyproject.toml from it; PyPilot won't manage conda directly.
```

## Two ways out

| Target | Command | Best when |
|---|---|---|
| `pyproject.toml` (uv or pip) | `pypilot migrate-conda` | The dependencies are mostly pure Python and exist on PyPI under the same names |
| `pixi.toml` (conda-forge + PyPI) | `pypilot migrate-conda --to pixi` | You rely on conda-only packages (gdal, cudatoolkit, compilers) or want to stay on conda-forge |

### To pyproject.toml

Pins are preserved, including conda's `=` prefix match, converted to `==X.Y.*`. It never overwrites an existing `pyproject.toml` and never deletes `environment.yml`.

```text
[project]
name = "conda"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = [
    "gdal",
    "numpy==1.26.*",
    "pip",
    "rich>=13",
]
```

Review the list before `pypilot setup`: conda names do not always match PyPI names, and some packages have no PyPI equivalent at all (here `gdal` would need a source build and `pip` is not a dependency).

### To pixi.toml

Keeps the channels and the conda packages, and moves the `pip:` section to `[pypi-dependencies]`. A preview unless `--write`. Full walkthrough in [Migrating to pixi](/pixi/migrate#step-by-step-a-conda-project).

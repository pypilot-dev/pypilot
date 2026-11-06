# Changelog

All notable changes to PyPilot's engine and Zed extension. Releases and their
full notes are on the [Releases page](https://github.com/pypilot-dev/pypilot/releases).

## [1.4.1]

- `pypilot setup` keeps a venv that already runs the Python it chose, and only syncs
  its packages. Before, it always rebuilt the venv, which on Windows failed with
  "Access is denied (os error 5)" whenever the editor was running the venv's Python.
- Before replacing a venv on Windows, PyPilot checks whether a program is running from
  it. If one is, it leaves the venv as it is and says what to close. Before,
  `uv venv --clear` deleted every installed package and then failed on the locked
  `Scripts` folder, leaving a broken environment.
- The Zed task templates are now `tasks/windows.json` and `tasks/macos-linux.json`,
  and they use the copy of `pypilot` the Zed extension downloaded when none is on
  `PATH`.

## [1.4.0]

`doctor --fix` now fixes what is actually wrong, instead of rebuilding the environment
for every problem. A healthy project is left alone, the Python version moves only when a
dependency forces it (and the output says which one), and every file PyPilot edits is
named.

- Targeted fixes, each only when a finding calls for it:
  - `uv sync --locked` for drift or a missing package, `uv lock` for a stale lock;
  - reinstalling only a half-installed package;
  - repairing a damaged venv on the Python it had;
  - raising `requires-python`;
  - editing one requirement, then re-resolving it. An edit that leaves the project
    unresolvable is rolled back.
- The environment is checked against the project: a stale `uv.lock`, packages missing,
  drifted or half-installed, a venv leaking system site-packages, a `VIRTUAL_ENV` that
  points at another project, and `requires-python` or `.python-version` that the venv
  breaks. An impossible `requires-python` (`>=3.16`, `>=3.10,<=3.9`) is an error, and
  nothing is built.
- The Python a fix chooses is written to `.python-version`, so a later `uv sync` keeps
  it. An existing pin wins unless a dependency forces a change.
- Conflicts are resolved before anything changes:
  - unsatisfiable pins get a relaxed pin, checked with uv before it is offered;
  - a typo gets "did you mean";
  - a version that does not exist gets the nearest releases;
  - a package with no build for this OS gets an environment marker (`uvloop` on Windows).
- A new data table, `known_breakages.json`, refreshed like the others, for breakages
  metadata does not show:
  - compiled packages that break under numpy 2 (pandas before 2.2.2);
  - uncapped dependencies that later removed an API: markupsafe/jinja2, werkzeug/flask,
    itsdangerous/flask, urllib3/requests-toolbelt, sqlalchemy/flask-sqlalchemy,
    importlib-metadata/flake8, and setuptools 82 and `pkg_resources`;
  - packages that shadow each other or the standard library (`jwt` and `PyJWT`, `enum34`,
    `typing`), and yanked pins;
  - stdlib modules removed in newer Pythons (`distutils`, `imp`, the 3.13 removals);
  - removed APIs used in the project's own code (`np.float`, `collections.Mapping`,
    `asyncio.coroutine`, `pydantic.BaseSettings`, SQLAlchemy 2 `engine.execute`, pandas 2
    `DataFrame.append`), reported by file and line with the replacement.
- More project layouts are handled with their own tools:
  - pixi: doctor's `pixi add python=X.Y` is applied, and a broken prefix is repaired;
  - a conda `environment.yml` moves to pixi and keeps its Python;
  - Poetry projects are read (`^`/`~` constraints, groups) and installed with Poetry;
  - an environment in `venv/` or `env/` is fixed in place.
- In the CLI, doctor reports undeclared imports (notebooks included) and adds the right
  distribution (`yaml` to PyYAML, `sklearn` to scikit-learn). `install` and `check` accept
  import names too. New command: `pypilot fix requirements`.
- requirements.txt lines with an obvious mistake are fixed, and the rest are reported by
  line number. requirements.txt no longer overwrites pyproject.toml pins; a disagreement
  between them is reported.
- `--dry-run` lists exactly what the real run does. `<mode> --help` prints that mode's
  help. A fix with a failed step no longer reports "Done".
- New `fix.kind` values in the JSON output: `sync_environment`, `relock`,
  `reinstall_packages`, `repair_environment`, `raise_requires_python`, `edit_requirement`,
  `add_requirements`, `replace_dist`, `migrate_conda_to_pixi` and `poetry_install`. Older
  kinds serialize as before.

## [1.3.0]

- pixi: `pixi_environment` and `--pixi-env` choose the environment; doctor checks
  every environment, Python drift between them, a stale `pixi.lock` (read-only) and
  PyPI dependencies with no wheel for a declared platform.
- A "pixi can help here" advisory in pip and uv projects (no wheel and native
  libraries, failed builds missing a header, CUDA sources without nvcc, mixed-language
  repos); it only suggests.
- `pypilot migrate-to-pixi`, `pypilot migrate-conda --to pixi`, `pypilot install-pixi`
  and `package_manager = "pixi"`; Zed's "Initialize pixi project" button.
- `pypilot explain`, `logs`, `completions` and `version`; `doctor --fix`;
  `update-data --check`.
- `--json` on every command, `--markdown` on `doctor` and `check`, `--dry-run` on
  `setup`, `install`, `fix` and `doctor --fix`.
- `diagnostics_scope = "workspace"`: import diagnostics for files that are not open.
- Releases carry `SHA256SUMS`, a CycloneDX SBOM and build-provenance attestations.
- `pypilot check` names the project dependency a package conflicts with, and suggests a
  conda-forge build when it fails.
- Editors' settings now reach the engine (VS Code's `pypilot.*` settings had no effect
  before); precedence is global file < editor < project file.
- Fixed: wheel resolution. A patch-level `requires-python` (`>=3.8.1`) no longer drops
  the whole minor version; 64-bit Windows no longer counts 32-bit `win32` wheels;
  free-threaded `cp3XXt` wheels no longer count for the regular build; manylinux and
  musllinux wheels are kept apart.
- Fixed: offline, or behind a refusing proxy, PyPilot no longer calls every package it
  could not look up a typo. The editor shows nothing for those imports, and doctor
  lists them as "not checked".
- Fixed: after `migrate-to-pixi`, doctor no longer warns that conda-forge packages from
  the old `requirements.txt` will compile from source.
- Dependencies updated for security advisories in rustls, tokio and zip.

## [1.2.2]

- Python 3.15 support. On a 3.15 project every package was reported as unsupported,
  even pure-Python ones and those shipping `cp315` wheels, and doctor recommended
  rebuilding on 3.14. 3.15 is now recognised for wheels, `requires-python`,
  interpreter discovery and recommendations.

## [1.2.1]

- Fixed: the environment stays on the Python PyPilot chose. uv rebuilds `.venv` on
  whatever `.python-version` (or a global `uv python pin`) asks for, so after setup or
  "Fix Python" the next `uv add` or `uv sync` could quietly move it to another version
  while the report said otherwise. New projects get `requires-python` and
  `.python-version` from the chosen Python, and rebuilding an existing uv project
  updates its pin.
- New projects are created with `uv init --bare`: only `pyproject.toml` is written, with
  no git repository, README, sample code or build system added to your folder.
- `pypilot check` and the editor's check report accept a requirement such as
  `mediapipe==0.10.14` or `numpy<2`, and judge the release it selects rather than the
  newest one.
- A misspelled package now reads "`x` is not on PyPI; check the spelling" instead of a
  raw 404.

## [1.2.0]

- pixi support: pixi projects are detected and managed through pixi (`pixi install`,
  `pixi add --pypi`, `pixi add python=X.Y`); PyPilot never creates a `.venv`, runs uv
  or touches `pixi.lock` in them, and does nothing destructive when pixi is missing.
- The pixi environment (`.pixi/envs/default`) is used for import diagnostics, installed
  packages, GPU checks and the Python version; conda packages count as installed.
- PyPI dependencies of a pixi project are checked against its Python, and pixi's install
  errors are explained with the package to blame and the `pixi add python=…` fix.
- Doctor respects the pixi `python` spec when recommending a version and lists conda and
  PyPI dependencies separately.
- Fixed: version specifiers follow PEP 440, so `==2.15.0` no longer matches
  `2.15.0.post1` (pinned releases were analysed against the wrong files).
- Fixed: a release with no files for this platform (for example Linux-only wheels on
  Windows) is reported as uninstallable here instead of supporting every Python.

## [1.1.2]

- Moved to pypilot-dev organization; updated download URLs.
  - The Zed extension now downloads the helper from `pypilot-dev/pypilot`, and its
    download permission in `extension.toml` names that path.
  - Installed engines refresh the bundled data tables from the new location.
  - Repository links in the Cargo manifests and the data checker point at the new home.
- Added CONTRIBUTING, Code of Conduct and issue templates.
- No change in behavior.

Older versions redirect, so extensions and engines installed before this release keep
working. Zed extensions older than 1.1.2 can only download releases from the old path,
so they keep using the helper they already have until Zed updates them.

# What doctor --fix fixes <Badge type="tip" text="1.4" />

`pypilot doctor --fix` (the **Fix environment** button in the editors) fixes what is wrong and nothing else. Before 1.4.0 it rebuilt the environment for every problem. Now each finding gets its own fix.

- A healthy project is left alone.
- The Python moves only when a dependency forces it, and the output says which one.
- An existing `.python-version` pin wins unless a dependency forces a change. The Python a fix picks is written to `.python-version`, so a later `uv sync` keeps it.
- Every file PyPilot edits is named in the output.
- An edit that leaves the project unresolvable is rolled back.
- Conflicts are resolved before anything changes, and each proposed edit is checked with uv first.

Run `pypilot doctor --fix --dry-run` to see exactly what the real run would do. See [Dry runs](/engine/dry-run).

## Problems and fixes

The last column is the `fix.kind` value in the [JSON output](/engine/output).

| Problem | What PyPilot does | Fix kind |
|---|---|---|
| Damaged venv (broken interpreter, system site-packages leaking in, `VIRTUAL_ENV` pointing at another project) | Repairs the venv on the Python it already had | `repair_environment` |
| Venv in `venv/` or `env/` | Fixes it in place | `repair_environment` |
| Package missing or drifted from the lock | `uv sync --locked` | `sync_environment` |
| Package half-installed | Reinstalls only that package | `reinstall_packages` |
| Stale `uv.lock` | `uv lock` | `relock` |
| `requires-python` or `.python-version` that the venv breaks | Rebuilds on a Python that fits, only when needed | `repair_environment` |
| A dependency needs a newer Python than `requires-python` allows | Raises `requires-python` and names the dependency | `raise_requires_python` |
| Unsatisfiable pins | Offers a relaxed pin, checked with uv, then re-resolves | `edit_requirement` |
| Typo in a package name | "Did you mean", then edits the requirement | `edit_requirement` |
| Version that does not exist | Names the nearest releases and edits the requirement | `edit_requirement` |
| No build for this OS (for example `uvloop` on Windows) | Adds an environment marker | `edit_requirement` |
| Compiled package that breaks under numpy 2 (pandas before 2.2.2) | Reports it and edits the requirement, then re-resolves | `edit_requirement` |
| Uncapped dependency that later removed an API (markupsafe/jinja2, werkzeug/flask, itsdangerous/flask, urllib3/requests-toolbelt, sqlalchemy/flask-sqlalchemy, importlib-metadata/flake8, setuptools 82 and `pkg_resources`) | Reports it and edits the requirement, then re-resolves | `edit_requirement` |
| Packages that shadow each other or the standard library (`jwt` and `PyJWT`, `enum34`, `typing`) | Replaces the package with the right distribution | `replace_dist` |
| Yanked pin | Edits the requirement | `edit_requirement` |
| Stdlib module removed in a newer Python (`distutils`, `imp`, the 3.13 removals) | Reports the module and the Python that removed it | none (report only) |
| Removed API used in your own code (`np.float`, `collections.Mapping`, `asyncio.coroutine`, `pydantic.BaseSettings`, SQLAlchemy 2 `engine.execute`, pandas 2 `DataFrame.append`) | Reports file, line and the replacement | none (report only) |
| Import with no declared package (`yaml`, `sklearn`) | Adds the right distribution (PyYAML, scikit-learn) | `add_requirements` |
| Obvious mistake in a `requirements.txt` line | Fixes the line. Reports the rest by line number | `add_requirements` |
| pixi project, wrong Python or broken prefix | Applies `pixi add python=X.Y`, repairs the prefix | see the output |
| conda `environment.yml` | Moves to pixi and keeps its Python | `migrate_conda_to_pixi` |
| Poetry project | Reads `^`/`~` constraints and groups, installs with Poetry | `poetry_install` |

`pypilot fix requirements` runs the requirements.txt and undeclared-import fixes on their own. `requirements.txt` no longer overwrites `pyproject.toml` pins. A disagreement between them is reported.

## Known breakages

Some breakages do not show in package metadata. They live in the `known_breakages.json` [data table](/reference/data-tables), which installed engines refresh from `main` every week. A new row reaches users without a release.

## What it will not do

- It never edits your source code. Removed APIs are reported with the replacement, and you make the change.
- It never touches your global uv Python pin.
- It does not run when `requires-python` is impossible, such as `>=3.16` or `>=3.10,<=3.9`. That is an error, and nothing is built.

## How well it works

PyPilot was run on 54 deliberately broken environments. 45 got the right targeted fix, none got a wrong one and none were missed. Version 1.3.0 got 13 right, 9 wrong and 21 missed on the same set.

# Changelog

All notable changes to PyPilot's engine and Zed extension. Releases and their
full notes are on the [Releases page](https://github.com/pypilot-dev/pypilot/releases).

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

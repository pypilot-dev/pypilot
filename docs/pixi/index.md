# pixi and PyPilot

[pixi](https://pixi.sh) is a package manager that installs from conda-forge and PyPI into a per-project environment, with a lock file. PyPilot supports pixi projects fully, and suggests pixi when a project hits a wall pip and uv cannot climb.

::: info What ships when
Engine **1.2.0+** detects pixi projects and works through pixi (`pixi install`, `pixi add --pypi`, `pixi add python=…`, the `.pixi/envs/default` environment). The rest of this guide, marked <Badge type="tip" text="1.3" /> where it matters (the multi-environment and lock checks, the advisory, `migrate-to-pixi`, `migrate-conda --to pixi`, `install-pixi`, `pixi_environment`, `package_manager = "pixi"`), needs engine 1.3.0 or newer.
:::

## Why pixi helps

pip and uv install Python packages. They cannot install what a source build needs outside Python, and a dependency without a wheel for your platform is a source build.

| Problem | With pip / uv | With pixi |
|---|---|---|
| **Native libraries.** GDAL, PROJ, HDF5, cairo, FFmpeg, eccodes… | The library must already be on the system, at the right version (`gdal-config` must match the bindings exactly) | conda-forge ships the library as a package next to the bindings |
| **Compilers and Fortran.** SciPy-stack packages without a wheel for your Python | A working C/C++/Fortran toolchain is needed | Prebuilt conda-forge binaries |
| **The CUDA toolkit.** Extensions with `.cu` sources need `nvcc` | Install the toolkit system-wide yourself | `cuda-toolkit` / `cuda-nvcc` from conda-forge, per project |
| **More than Python.** R, Rust, CMake/Meson C++ next to Python | Separate tools | One environment, one manifest |
| **Reproducibility** | uv has `uv.lock`; plain pip has none | `pixi.lock` for every platform listed in the manifest |

pixi also installs Python itself, so the interpreter is part of the environment.

## What PyPilot does in pixi projects

A folder is a pixi project when it has a `pixi.toml`, or a `pyproject.toml` with `[tool.pixi.workspace]`. PyPilot then works only through pixi, whatever [`package_manager`](/reference/configuration#package-manager) says. Details in [PyPilot in pixi projects](./projects).

- The environment is `.pixi/envs/<name>` (`default` unless [`pixi_environment`](/reference/configuration#pixi-environment) says otherwise). Its Python, packages and GPU builds are read from there.
- Setup runs `pixi install`, installing a package runs `pixi add --pypi`, changing Python runs `pixi add python=X.Y`.
- No `.venv` is created, uv is not run, and `pixi.lock` is never written by PyPilot.
- doctor checks every environment, the lock, and every target platform.

<TerminalDemo src="doctor-pixi" cmd="pypilot doctor" title="~/src/px" />

## When PyPilot suggests pixi <Badge type="tip" text="1.3" />

Outside pixi projects, PyPilot raises a **"pixi can help here"** finding when it sees a dependency with no installable wheel whose build needs native libraries that conda-forge provides, a failed build naming a missing header or library, CUDA sources without `nvcc`, or a repo that mixes Python with R, Rust or C++. See [the advisory](./advisory).

## Moving a project to pixi <Badge type="tip" text="1.3" />

`pypilot migrate-to-pixi` (from pip/uv) and `pypilot migrate-conda --to pixi` (from `environment.yml`) preview a `pixi.toml` and write it only when asked. Your existing files are never edited or deleted. Step by step: [Migrating to pixi](./migrate).

## No pixi installed?

PyPilot says so, changes nothing, and offers to install it: `pypilot install-pixi`, **PyPilot: Install pixi** in VS Code, or the **Install pixi** button in the notification. It runs pixi's official installer.

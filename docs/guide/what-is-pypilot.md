# What is PyPilot?

You clone a Python repository, run the install, and it fails because a dependency has no prebuilt wheel for your interpreter. PyPilot works out which Python versions the project's dependencies actually support, sets up an environment on one of them, and names the package that set the limit when there is none.

It answers from PyPI package metadata and a few local lookup tables. There is no AI service, no account and no API key; everything runs on your machine.

## The problem

A package's `requires_python` is often incomplete. mediapipe 0.10.14 does not declare it at all, yet only ships wheels for cp39 through cp312. On Python 3.13 pip looks for a source distribution that does not exist and stops with an error that never mentions the Python version.

PyPilot reads both signals. It takes `requires_python` as the starting range and intersects it with the CPython versions that have a wheel for your OS and CPU. Doing that for every dependency gives the interpreters the whole project can run on. When the intersection is empty, it names the packages that conflict and their ranges, because a dependency collision is where package-manager errors help least.

<TerminalDemo src="explain-pkg" cmd="pypilot explain mediapipe==0.10.14" />

## What else it checks

- **GPU builds.** For pip-installed torch and tensorflow the NVIDIA *driver* decides what works, not the system CUDA toolkit. PyPilot reads `nvidia-smi`, maps the driver to the newest CUDA runtime it supports and picks the matching PyTorch index. See [GPU & CUDA](./gpu-cuda).
- **Pinned versions.** `mediapipe==0.10.14` is judged against that release's wheels, not the newest release's.
- **Imports.** In the editor, imports that are missing or cannot run on the environment's Python get a diagnostic and a quick fix. Attributes removed from the installed release (`mp.solutions` in mediapipe 0.10.35) are flagged too. See [Findings](/reference/checks).
- **Notebooks.** Cells in `.ipynb` files get the same diagnostics, and PyPilot keeps a working `ipykernel` in the environment. See [Notebooks](./notebooks).
- **Native libraries.** <Badge type="tip" text="1.3" /> When a dependency needs C libraries, Fortran or nvcc that pip cannot install, and conda-forge has a build, PyPilot says pixi can help and can migrate the project. See [pixi](/pixi/).

## The products

| Product | What it is | Where |
|---|---|---|
| **Engine** | The `pypilot` binary, written in Rust: CLI and language server. All the logic lives here. | [pypilot-dev/pypilot](https://github.com/pypilot-dev/pypilot) `helper/` |
| **Zed extension** | A WebAssembly shim that downloads the engine and registers it as a language server. | same repo, `extension/` |
| **VS Code extension** | Downloads and manages the engine, adds commands, a report panel, a status bar item, tasks and interpreter selection. | [pypilot-dev/pypilot-vscode](https://github.com/pypilot-dev/pypilot-vscode) |
| **pypilot-cli** | `pip install pypilot-cli`: a small Python package that runs the engine (bundled in its platform wheels, or downloaded), plus a tiny Python API. | [pypilot-dev/pypilot-pip](https://github.com/pypilot-dev/pypilot-pip) |

Because the editor extensions hold no business logic, the terminal and the editors cannot disagree: the "Fix environment" button and `pypilot setup` call the same function.

## What it is not

- Not a package manager. It drives uv, pip or pixi; it does not replace them.
- Not a resolver of transitive conflicts in general. It reasons about Python versions and platforms from metadata. In uv mode it also asks `uv --dry-run`, and reports the real resolver conflict when that disagrees with the metadata verdict.
- Not a conda manager. Conda projects get a [migration path](./conda), not management.

Next: [Getting started](./getting-started).

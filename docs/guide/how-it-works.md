# How it works

<ArchDiagram />

## One engine, thin wrappers

- The **engine** (`pypilot`, Rust) does everything: probing the machine, parsing dependency files, fetching PyPI metadata, the version arithmetic, GPU matching, running uv/pip/pixi, and the language server.
- The **Zed extension** is a WebAssembly shim. It uses a `pypilot` on `PATH` if it is new enough, otherwise downloads the release asset for your platform from GitHub, and registers it with Zed as a language server for Python.
- The **VS Code extension** resolves the engine the same way (PATH, then its own managed copy, then a download), starts it with `pypilot lsp`, and adds VS Code surfaces on top.
- **pypilot-cli** on PyPI downloads the engine release it pins and forwards your command line to it.

## The assessment

When you open a project (with `auto_check_on_open = true`, the default) or run `pypilot doctor`, the engine:

1. **Probes the machine**: the project environment (`.venv` or the pixi environment), whether uv and pixi are installed, which interpreters exist, and the NVIDIA driver through `nvidia-smi`.
2. **Reads the project**: `pyproject.toml`, `requirements.txt`, `pixi.toml` (or `[tool.pixi.workspace]`), `environment.yml`, including version pins. In a worktree whose Python code lives in a single subfolder, that subfolder is the project.
3. **Fetches PyPI metadata** for each dependency: the release a pin selects (or the newest), its `requires_python`, and the tags of every file.
4. **Intersects**: for each dependency, the CPython versions allowed by `requires_python` and installable on this platform (a wheel, or an sdist to build). Across all dependencies, the intersection is what the project runs on. The newest version in it is recommended, within the project's own `requires-python`.
5. **Adds findings**: a missing or wrong-Python environment, conflicts, unresolvable names, source builds, CUDA matches, conda files, pixi advice, and more. See [Findings](/reference/checks).

<TerminalDemo src="explain-project" cmd="pypilot explain" title="~/src/demo" />

## Caching and the network

- PyPI metadata is cached on disk in the platform cache directory. A specific release's file list never changes, so it is cached forever; "what is the newest release" is cached for 24 hours.
- The data tables (`nvidia.json`, `frameworks.json`, `cuda_extensions.json`, `import_map.json`, `conda_forge_packages.json`) are compiled into the engine and refreshed from the repository's `main` branch every `data_refresh_days` (7 by default). Set it to `0` to stay offline. See [Data tables](/reference/data-tables).
- Nothing else leaves your machine. There is no telemetry.

## Two surfaces in the editor

- **The workspace scan** runs once per relevant event (opening the folder, saving a dependency file, running a task) and raises at most one notification about the environment as a whole. A correct environment stays silent.
- **Live diagnostics** run on each Python buffer (and notebook cell) as you type, debounced by a second, and immediately on open and save. They look at imports: missing, unsupported on this Python, or attributes the installed release does not export.

A CLI run started from a Zed task leaves a small request file in the cache directory; the running language server notices it and re-raises the notification, which is how a terminal command shows up in the editor.

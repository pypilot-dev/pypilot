# Python API

`pypilot-cli` also exposes a small API for running the engine from Python. It is typed (`py.typed`).

```python
import pypilot

result = pypilot.run(["doctor"], capture_output=True)
print(result.returncode)
print(result.stdout)

engine = pypilot.ensure_binary()
print(f"engine at {engine}")
```

## `pypilot.run(args=None, capture_output=False)`

Run the engine with `args` (a list of strings, for example `["check", "torch"]`). Returns a `subprocess.CompletedProcess[str]`; output is text. It never raises on a non-zero exit status (`check=False`): inspect `returncode` yourself. Finds or downloads the engine first, like the CLI.

```python
import json
import pypilot

# `explain --json` needs an engine newer than 1.2.2 (next release).
res = pypilot.run(["explain", "--json", "--path", "path/to/project"], capture_output=True)
explanation = json.loads(res.stdout)["explanation"]
print(explanation["supported_range"], explanation["recommendation"])
```

## `pypilot.ensure_binary() -> pathlib.Path`

Return the path of a usable engine, downloading the pinned release if needed. The search order is described in [Which engine it runs](/python/#which-engine-it-runs). Raises `RuntimeError` on an unsupported OS or architecture, and network errors from the download propagate.

## `pypilot.get_cache_dir() -> pathlib.Path`

The directory where the engine is cached (created if missing): `%LOCALAPPDATA%\pypilot\bin` on Windows, `$XDG_CACHE_HOME/pypilot/bin` or `~/.cache/pypilot/bin` elsewhere.

## `pypilot.__version__`

The package version, for example `"1.2.2"`.

## `pypilot.binary`

Lower-level helpers live in `pypilot.binary`. They are not part of `__all__` and may change: `DEFAULT_TAG` (the pinned engine tag, `"v1.2.2"`), `get_platform_slug()`, `download_binary(tag=DEFAULT_TAG)`, `probe_binary(path)` (runs `--version`), `parse_version(text)`.

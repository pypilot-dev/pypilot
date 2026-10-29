# Python API

`pypilot-cli` also exposes a small API for running the engine from Python. It is typed (`py.typed`). Items marked <Badge type="tip" text="next" /> are on `master` and ship in the next release.

```python
import pypilot

result = pypilot.run(["doctor"], capture_output=True)
print(result.returncode)
print(result.stdout)

# next: parse --json output, raise on failure, find the engine
report = pypilot.run(["doctor", "--json"], capture_output=True).json()
pypilot.run(["check", "torch"], check=True)
info = pypilot.locate_engine()      # EngineInfo(path, source, version)
```

## `pypilot.run(...)`

```python
pypilot.run(args=None, capture_output=False, check=False, *,
            cwd=None, env=None, input=None, timeout=None) -> RunResult
```

Run the engine with `args`, a list of strings such as `["check", "torch"]` (a plain string raises `TypeError`). Finds or downloads the engine first, like the CLI. Output is decoded as UTF-8, which is what the engine writes, whatever the locale.

| Parameter | Meaning |
|---|---|
| `capture_output` | Capture stdout and stderr instead of leaving them on the terminal |
| `check` <Badge type="tip" text="next" /> | Raise `subprocess.CalledProcessError` on a non-zero exit. Default `False`: inspect `returncode` yourself |
| `cwd`, `env`, `input`, `timeout` <Badge type="tip" text="next" /> | As for `subprocess.run`. `cwd` is an easy way to point the engine at a project without `--path` |

Version 1.2.2 has `run(args=None, capture_output=False)` only, returning a plain `subprocess.CompletedProcess[str]`.

```python
import pypilot

res = pypilot.run(["explain", "--json"], capture_output=True, cwd="path/to/project")
explanation = res.json()["explanation"]
print(explanation["supported_range"], explanation["recommendation"])
```

`--json` and `explain` need an engine newer than 1.2.2; see [Machine-readable output](/engine/output).

## `RunResult` <Badge type="tip" text="next" />

A subclass of `subprocess.CompletedProcess[str]`, so `.args`, `.returncode`, `.stdout`, `.stderr` and `.check_returncode()` work as before. It adds:

- `.ok`: `returncode == 0`.
- `.json()`: parse stdout as the command's `--json` document (`{"pypilot", "command", "ok", …}`). Raises `ValueError` if stdout was not captured.

Note that `doctor` and `check` exit `0` even when they report problems; read `res.json()["ok"]` for their verdict.

## `await pypilot.arun(...)` <Badge type="tip" text="next" />

The same parameters and result as `run`, without blocking the event loop. Locating (and possibly downloading) the engine happens in a worker thread. On timeout, or when the task is cancelled, the engine process is killed; a timeout raises `subprocess.TimeoutExpired`.

```python
import asyncio, pypilot

async def main():
    res = await pypilot.arun(["doctor", "--json"], capture_output=True, timeout=120)
    print(res.json()["ok"])

asyncio.run(main())
```

## Finding the engine

### `pypilot.ensure_binary() -> pathlib.Path`

Return the path of a usable engine, downloading the pinned release if needed and allowed. The search order is described in [Which engine it runs](/python/#which-engine-it-runs).

### `pypilot.locate_engine() -> EngineInfo` <Badge type="tip" text="next" />

The same search, with where the engine came from: `EngineInfo(path, source, version)`, a frozen dataclass. `source` is `override`, `bundled`, `cache`, `cargo`, `path` or `download`; `version` is a `(major, minor, patch)` tuple, or `None` when it could not be read.

### `pypilot.get_cache_dir() -> pathlib.Path`

The directory where downloaded engines are cached (created if missing): `%LOCALAPPDATA%\pypilot\bin` on Windows, `$XDG_CACHE_HOME/pypilot/bin` or `~/.cache/pypilot/bin` elsewhere. The next release keeps one subfolder per engine tag in it.

## Errors <Badge type="tip" text="next" />

Engine problems raise subclasses of `pypilot.EngineError` (itself a `RuntimeError`):

| Exception | When |
|---|---|
| `EngineNotFoundError` | No engine is available and none may be downloaded (`PYPILOT_OFFLINE=1`) |
| `EngineDownloadError` | A release file could not be fetched; `.status` holds the HTTP status when there was one |
| `EngineVerificationError` | A download did not match its expected SHA-256, or could not be checked |

An unsupported OS or architecture raises `RuntimeError`. In 1.2.2, network errors from the download propagate as they are.

## `pypilot.__version__`

The package version, for example `"1.2.2"`.

## `pypilot.binary`

Lower-level names live in `pypilot.binary` and may change. `DEFAULT_TAG` (the pinned engine tag, `"v1.2.2"`) and, <Badge type="tip" text="next" />, `ENGINE_SHA256` (the hash of each archive of that release) are the two assignments the engine's release automation bumps. The helpers `get_platform_slug()`, `download_binary(tag=DEFAULT_TAG)`, `probe_binary(path)` (runs `--version`) and `parse_version(text)` stay importable from here; in the next release they live in `pypilot.engine`.

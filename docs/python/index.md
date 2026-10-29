# Python package: pypilot-cli

`pypilot-cli` on PyPI puts the `pypilot` command on any machine with Python, including CI runners. It is a thin wrapper: on first use it downloads the native engine for your platform and then forwards every command line to it.

- [PyPI](https://pypi.org/project/pypilot-cli/) · [Source](https://github.com/pypilot-dev/pypilot-pip)
- Current version **1.2.2**, which pins engine **v1.2.2**. Runs on Python 3.8 and later. No dependencies.

## Install

```bash
pip install pypilot-cli
uv tool install pypilot-cli     # isolated, on PATH
pipx install pypilot-cli        # isolated, on PATH
```

Then use it exactly like the engine; every mode and flag on the [command reference](/engine/commands) works:

```bash
pypilot doctor
pypilot setup
pypilot check mediapipe==0.10.14
```

The exit status is the engine's. `Ctrl+C` exits with 130.

## Which engine it runs

1. The cached engine in the cache directory, if it is at least the pinned version.
2. `~/.cargo/bin/pypilot`, if it is 1.0.2 or newer.
3. A `pypilot` on `PATH` (never the Python entry point itself), if it is 1.0.2 or newer.
4. Otherwise it downloads the pinned release, `pypilot-<platform>.<ext>` from `github.com/pypilot-dev/pypilot/releases/download/v1.2.2/`, into the cache directory, printing `PyPilot: downloading native engine from … ...` on stderr.

| Platform | Asset | Cache directory |
|---|---|---|
| Linux x86-64 | `pypilot-linux-x64.tar.gz` | `$XDG_CACHE_HOME/pypilot/bin`, else `~/.cache/pypilot/bin` |
| macOS Apple Silicon | `pypilot-darwin-arm64.tar.gz` | `~/.cache/pypilot/bin` (or `$XDG_CACHE_HOME`) |
| macOS Intel | `pypilot-darwin-x64.tar.gz` | same |
| Windows x64 | `pypilot-windows-x64.zip` | `%LOCALAPPDATA%\pypilot\bin` |

Other platforms (Linux arm64, Windows arm64) raise an "Unsupported … architecture" error: [build the engine](/engine/building) and put it on `PATH`, and the wrapper uses it.

Because the pinned version moves with the package, `pip install -U pypilot-cli` also upgrades the engine.

## Offline machines

Copy a `pypilot` binary (from a release archive, or `cargo install`) onto `PATH` or into the cache directory above. The wrapper then never touches the network; set `data_refresh_days = 0` to keep the engine offline too. See [Security & release integrity](/guide/security#offline-use).

## Coming in the next release

<!-- verify after merge -->

- **Bundled platform wheels**: platform-specific wheels of `pypilot-cli` carry the engine inside, so `pip install` needs no second download (and works where GitHub is blocked).
- **Hash verification**: the download is checked against the release's `SHA256SUMS` before use.

See also the [Python API](./api).

# Python package: pypilot-cli

`pypilot-cli` on PyPI puts the `pypilot` command on any machine with Python, including CI runners. It is a thin wrapper around the native engine: it finds (or downloads) the engine for your platform and forwards every command line to it.

- [PyPI](https://pypi.org/project/pypilot-cli/) · [Source](https://github.com/pypilot-dev/pypilot-pip)
- Current version **1.2.2**, which pins engine **v1.2.2**. Runs on Python 3.8 and later. No dependencies.
- Items marked <Badge type="tip" text="next" /> are merged and ship in the next release.

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

## Wheels with the engine inside <Badge type="tip" text="next" /> {#wheels}

The next release publishes platform wheels that carry the engine, so `pip install` is all it takes and nothing is downloaded at run time (it also works where GitHub is blocked). pip picks the right one:

| Platform | Wheel tag | Engine |
|---|---|---|
| Linux x86-64, glibc 2.34 or newer (Ubuntu 22.04+, Debian 12+, RHEL/Alma/Rocky 9+, Fedora 35+) | `manylinux_2_34_x86_64` | bundled |
| macOS 11+ on Apple Silicon | `macosx_11_0_arm64` | bundled |
| macOS 10.12+ on Intel | `macosx_10_12_x86_64` | bundled |
| Windows x64 | `win_amd64` | bundled |
| Anything else (older glibc, other CPUs) | `py3-none-any` | downloaded on first use, if a release exists for the platform |

The bundled binaries are the pinned release's archives, verified against the same hashes as a download. There is no musl (Alpine) build of the engine. On Linux with an older glibc, or on arm64, [build the engine](/engine/building) (`cargo install --path helper --locked`) and the wrapper finds it in `~/.cargo/bin`.

Version 1.2.2 is a pure-Python wheel only, and always downloads the engine.

## Which engine it runs

### Next release <Badge type="tip" text="next" />

The first match wins:

1. `PYPILOT_ENGINE_VERSION=vX.Y.Z`, if set: that engine release, from the cache or downloaded and checked against the release's `SHA256SUMS`.
2. The engine bundled in a platform wheel (`site-packages/pypilot/_engine/`).
3. An earlier verified download of the pinned release, in the cache (`<cache>/<tag>/`).
4. `~/.cargo/bin/pypilot`, if it is 1.0.2 or newer.
5. A `pypilot` engine on `PATH` (never the wrapper's own launcher), if it is 1.0.2 or newer.
6. Otherwise a download of the pinned release, unless offline mode is on.

### 1.2.2

1. The cached engine, if it is at least the pinned version.
2. `~/.cargo/bin/pypilot`, if it is 1.0.2 or newer.
3. A `pypilot` on `PATH` (never the Python entry point itself), if it is 1.0.2 or newer.
4. Otherwise it downloads the pinned release, `pypilot-<platform>.<ext>` from `github.com/pypilot-dev/pypilot/releases/download/v1.2.2/`, into the cache directory, printing `PyPilot: downloading native engine from … ...` on stderr.

| Platform | Asset | Cache directory |
|---|---|---|
| Linux x86-64 | `pypilot-linux-x64.tar.gz` | `$XDG_CACHE_HOME/pypilot/bin`, else `~/.cache/pypilot/bin` |
| macOS Apple Silicon | `pypilot-darwin-arm64.tar.gz` | `~/.cache/pypilot/bin` (or `$XDG_CACHE_HOME`) |
| macOS Intel | `pypilot-darwin-x64.tar.gz` | same |
| Windows x64 | `pypilot-windows-x64.zip` | `%LOCALAPPDATA%\pypilot\bin` |

The next release keeps one subfolder per engine tag in that directory, so two installs pinned to different engines never overwrite each other. Other platforms (Linux arm64, Windows arm64) raise an "Unsupported … architecture" error: [build the engine](/engine/building) and put it on `PATH`, and the wrapper uses it.

Because the pinned version moves with the package, `pip install -U pypilot-cli` also upgrades the engine.

## Verification <Badge type="tip" text="next" /> {#verification}

Each `pypilot-cli` release pins one engine release (`DEFAULT_TAG` in `pypilot/binary.py`) and the SHA-256 of each of its four archives (`ENGINE_SHA256`). A download is checked **before** it is extracted or run. On a mismatch the file is deleted and the command fails with `SHA-256 mismatch`.

With `PYPILOT_ENGINE_VERSION`, the archive is checked against that release's `SHA256SUMS` file instead. A release without one (engine 1.2.2 and older) is refused unless you also set `PYPILOT_ALLOW_UNVERIFIED=1`, which prints a warning.

Version 1.2.2 downloads over HTTPS without a hash check. See [Security & release integrity](/guide/security#what-each-wrapper-downloads-and-checks).

## `pypilot --version` <Badge type="tip" text="next" /> {#version}

A lone `--version` (or `-V`) is answered by the wrapper. stdout carries only the engine's own line, because the editor extensions parse exactly that; the wrapper's line goes to stderr:

```text
pypilot 1.2.2
pypilot-cli 1.2.2 (engine: bundled, …/site-packages/pypilot/_engine/pypilot)
```

The source is `bundled`, `cache`, `download`, `cargo`, `path` or `override` (from `PYPILOT_ENGINE_VERSION`). A `warning:` line follows on stderr when the engine is older than the pinned one, or when it comes from `~/.cargo/bin` or `PATH` with a different version. Every other argument, including `pypilot version` and `pypilot completions <shell>`, goes to the engine unchanged.

## Offline and proxies <Badge type="tip" text="next" /> {#offline}

- `PYPILOT_OFFLINE=1` forbids every download. If no engine is available locally, the command fails at once and says so, without waiting on the network. With a platform wheel, offline mode just works.
- Downloads use the standard proxy settings: `HTTPS_PROXY` / `HTTP_PROXY` and `NO_PROXY` (either case), or, when none is set, the system settings (Windows Internet Options, macOS network settings). A failed download says which proxy it went through.
- `PYPILOT_DOWNLOAD_TIMEOUT` sets the network timeout in seconds (default 60).

These control the wrapper's engine download only. The engine's own network use (PyPI metadata, data tables) is configured separately: see [offline use](/guide/security#offline-use).

With 1.2.2, copy a `pypilot` binary (from a release archive, or `cargo install`) onto `PATH` or into the cache directory above; the wrapper then never touches the network.

### Environment variables {#environment-variables}

| Variable | Effect |
|---|---|
| `PYPILOT_OFFLINE=1` | Never download; fail fast if no engine is available |
| `PYPILOT_ENGINE_VERSION=vX.Y.Z` | Use this engine release instead of the pinned one, verified against its `SHA256SUMS` |
| `PYPILOT_ALLOW_UNVERIFIED=1` | Allow a `PYPILOT_ENGINE_VERSION` release that has no `SHA256SUMS` |
| `PYPILOT_DOWNLOAD_TIMEOUT=<seconds>` | Network timeout for downloads (default 60) |
| `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` | Proxy for downloads, and hosts that bypass it |

All are <Badge type="tip" text="next" />. The engine's own variables (`PYPILOT_NO_LOG`, `PYPILOT_LOG_DIR`) pass through unchanged; see [Logs](/engine/logs).

## Supply chain <Badge type="tip" text="next" />

Each GitHub release of `pypilot-cli` carries a CycloneDX SBOM (`pypilot-cli.cdx.json`) that lists the bundled engine archives with their hashes. CI and the publish job run `pip-audit` on the locked dependencies.

See also the [Python API](./api).

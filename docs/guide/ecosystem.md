# Ecosystem & compatibility

Three wrappers, one engine. The wrappers decide *which* engine runs; the engine decides everything else.

## Compatibility matrix

| Component | Version | Engine it runs | Python / platform |
|---|---|---|---|
| Engine (`pypilot`) | **1.2.2** | — | Analyzes CPython 3.7–3.15. Prebuilt for Linux x64, macOS x64/arm64, Windows x64 |
| Zed extension | **1.2.2** (this repo) | The newest engine release on GitHub, or a `pypilot` on `PATH` | Zed with WASI extensions |
| VS Code extension | **1.2.1** | The newest engine release it finds, unless a newer `pypilot` is on `PATH` | VS Code 1.85+ |
| `pypilot-cli` (PyPI) | **1.2.2** | Pins engine **v1.2.2**; a `pypilot` in `~/.cargo/bin` or on `PATH` is used if ≥ 1.0.2 | Python 3.8+ |
| **next** engine | *next* | — | Adds the features marked <Badge type="tip" text="next" /> |
| **next** VS Code | *next* | Pins an engine version and verifies its SHA-256 (`pypilotEngine` in `package.json`, currently 1.2.2); `pypilot.engineUpdates: "latest"` also offers newer releases | VS Code 1.85+ |
| **next** pypilot-cli | *next* | Pins `DEFAULT_TAG` with SHA-256 hashes (`ENGINE_SHA256`) <!-- verify after merge --> | Python 3.8+ |

Throughout this site, <Badge type="tip" text="next" /> marks something that is merged but not yet released.

## How each wrapper picks an engine

### Zed

1. A `pypilot` on `PATH` whose version is new enough.
2. Otherwise the newest non-prerelease GitHub release of `pypilot-dev/pypilot` with an asset for your platform, cached per version. If GitHub is unreachable, a previously downloaded copy is used.

### VS Code

1. A `pypilot` on `PATH`, and a previously downloaded copy in the extension's storage. If both exist, the newer one wins.
2. Otherwise the latest release is downloaded.

On each start the extension compares the engine against the latest release and offers **Update now**, **Release notes** or **Later** (a dismissed prompt stays quiet for 24 hours). An update always goes into the extension's own storage; a copy on `PATH` is yours and is never touched.

<Badge type="tip" text="next" /> `pypilot.helperPath` comes first when set, and downloads are the pinned version, verified. See [VS Code](/vscode/#a-pinned-verified-engine).

### pypilot-cli

1. Its cached engine, if it is at least the pinned version (`DEFAULT_TAG`).
2. `~/.cargo/bin/pypilot`, then a `pypilot` on `PATH` (not the Python script itself), if at least 1.0.2.
3. Otherwise it downloads the pinned release into the cache directory.

Upgrading the pip package therefore upgrades the engine.

## Release assets

| Asset | Platform |
|---|---|
| `pypilot-linux-x64.tar.gz` | Linux x86-64 |
| `pypilot-darwin-x64.tar.gz` | macOS Intel |
| `pypilot-darwin-arm64.tar.gz` | macOS Apple Silicon |
| `pypilot-windows-x64.zip` | Windows x64 |
| `SHA256SUMS`, `pypilot-helper.cdx.json` <Badge type="tip" text="next" /> | Checksums and SBOM, with provenance attestations |

## Release order

1. **Engine**: version bump, tag `vX.Y.Z`; the Release workflow uploads the assets.
2. **pypilot-cli**: points its pin at that tag, then publishes to PyPI. PyPI never allows reusing a version number.
3. **VS Code extension**: independent of the engine version (its engine pin is bumped by a pull request after each engine release).
4. **Zed registry**: the registry entry is bumped to the engine release commit.

Data-table changes need no release: installed engines refresh them from `main`. See [Data tables](/reference/data-tables).

## Changelogs

- Engine and Zed extension: [CHANGELOG.md](https://github.com/pypilot-dev/pypilot/blob/main/CHANGELOG.md) and [Releases](https://github.com/pypilot-dev/pypilot/releases)
- VS Code: [CHANGELOG.md](https://github.com/pypilot-dev/pypilot-vscode/blob/master/CHANGELOG.md)
- pypilot-cli: [pypilot-pip](https://github.com/pypilot-dev/pypilot-pip)

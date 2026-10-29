# Ecosystem & compatibility

Three wrappers, one engine. The wrappers decide *which* engine runs; the engine decides everything else.

## Compatibility matrix

| Component | Version | Engine it runs | Python / platform |
|---|---|---|---|
| Engine (`pypilot`) | **1.3.0** | n/a | Analyzes CPython 3.7–3.15. Prebuilt for Linux x64, macOS x64/arm64, Windows x64 |
| Zed extension | **1.3.0** (this repo) | The newest engine release on GitHub, or a `pypilot` on `PATH` | Zed with WASI extensions |
| VS Code extension | **1.3.0** | `pypilot.helperPath` if set; else the newer of a `pypilot` on `PATH` and its own copy; else downloads the version pinned in `package.json` (`pypilotEngine`, engine 1.3.0), verified by SHA-256. `pypilot.engineUpdates: "latest"` also offers newer releases, verified against their `SHA256SUMS` | VS Code 1.85+ |
| `pypilot-cli` (PyPI) | **1.3.0** | Pins engine **v1.3.0** and bundles it in its platform wheels (Windows x64, Linux x64 with glibc 2.34+, macOS arm64 and x64); elsewhere, a download verified against `ENGINE_SHA256`. `PYPILOT_ENGINE_VERSION` selects another release, verified against its `SHA256SUMS` | Python 3.8+ |

The features marked <Badge type="tip" text="1.3" /> (`--json`, `--dry-run`, `diagnosticsScope`, …) need engine 1.3.0 or newer, which all three wrappers now run by default. To try something newer than the latest release, build the engine from `main` and put it on `PATH` (Zed, VS Code) or in `pypilot.helperPath` (VS Code); see [Building from source](/engine/building).

Throughout this site, <Badge type="tip" text="1.3" /> marks something new in 1.3.

## How each wrapper picks an engine

### Zed

1. A `pypilot` on `PATH` whose version is new enough.
2. Otherwise the newest non-prerelease GitHub release of `pypilot-dev/pypilot` with an asset for your platform, cached per version. If GitHub is unreachable, a previously downloaded copy is used.

### VS Code

1. `pypilot.helperPath`, when set: that binary and nothing else (no download, no update offers).
2. A `pypilot` on `PATH`, and a previously downloaded copy in the extension's storage. If both exist, the newer one wins.
3. Otherwise the engine version pinned in `package.json` is downloaded and verified by SHA-256. See [VS Code](/vscode/#a-pinned-verified-engine).

On each start the extension offers the pinned version when it is newer than the engine in use (with `pypilot.engineUpdates: "latest"`, newer GitHub releases too), with **Update now**, **Release notes** or **Later** (a dismissed prompt stays quiet for 24 hours). An update always goes into the extension's own storage; a copy on `PATH` is yours and is never touched.

### pypilot-cli

1. `PYPILOT_ENGINE_VERSION`, if set.
2. The engine bundled in the platform wheel.
3. A verified cached download of the pinned release (`DEFAULT_TAG`).
4. `~/.cargo/bin/pypilot`, then a `pypilot` on `PATH` (not the Python script itself), if at least 1.0.2.
5. Otherwise a verified download of the pinned release, unless `PYPILOT_OFFLINE=1`.

See [Which engine it runs](/python/#which-engine-it-runs).

Upgrading the pip package therefore upgrades the engine.

## Release assets

| Asset | Platform |
|---|---|
| `pypilot-linux-x64.tar.gz` | Linux x86-64 |
| `pypilot-darwin-x64.tar.gz` | macOS Intel |
| `pypilot-darwin-arm64.tar.gz` | macOS Apple Silicon |
| `pypilot-windows-x64.zip` | Windows x64 |
| `SHA256SUMS`, `pypilot-helper.cdx.json` <Badge type="tip" text="1.3" /> | Checksums and SBOM, with provenance attestations |

## Release order

1. **Engine**: version bump, tag `vX.Y.Z`; the Release workflow uploads the assets.
2. **pypilot-cli**: points its pin at that tag, then publishes to PyPI. PyPI never allows reusing a version number.
3. **VS Code extension**: independent of the engine version (its engine pin is bumped by a pull request after each engine release).

Steps 2 and 3 start from the pin-bump pull requests the engine's `ecosystem-bump` workflow opens in each wrapper repository once the Release run succeeds. See [Security](./security#what-each-wrapper-downloads-and-checks).
4. **Zed registry**: the registry entry is bumped to the engine release commit.

Data-table changes need no release: installed engines refresh them from `main`. See [Data tables](/reference/data-tables).

## Changelogs

- Engine and Zed extension: [CHANGELOG.md](https://github.com/pypilot-dev/pypilot/blob/main/CHANGELOG.md) and [Releases](https://github.com/pypilot-dev/pypilot/releases)
- VS Code: [CHANGELOG.md](https://github.com/pypilot-dev/pypilot-vscode/blob/master/CHANGELOG.md)
- pypilot-cli: [pypilot-pip](https://github.com/pypilot-dev/pypilot-pip)

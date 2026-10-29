# Security & release integrity

## What runs where

- The engine is one native binary. It never runs a shell: every command it spawns (uv, pip, pixi, `nvidia-smi`) is an argument vector, so paths with spaces or quotes cannot be reinterpreted. `install-pixi` downloads pixi's official installer script to a file first and runs that file, rather than piping the network into a shell; pixi's own `PIXI_HOME`, `PIXI_VERSION` and `PIXI_NO_PATH_UPDATE` variables are honoured.
- It reads your project files and PyPI metadata, and runs package managers only when you ask it to change something (`setup`, `install`, `fix`, a quick fix, a notification button).
- Network access: PyPI (package metadata), GitHub (engine releases, data tables from `raw.githubusercontent.com`), prefix.dev (the PyPI-to-conda name mapping used by the pixi migration), and pixi's installer (`pixi.sh`) only when you run `install-pixi`. No telemetry.
- <Badge type="tip" text="next" /> It keeps a local log of what it ran, in the cache directory, with credentials masked; nothing in it leaves the machine. See [Logs](/engine/logs#the-local-log).

## Where releases come from

Engine releases are built by the [Release workflow](https://github.com/pypilot-dev/pypilot/blob/main/.github/workflows/release.yml) on GitHub-hosted runners from the tagged commit, with `--locked` so exactly `Cargo.lock` is used. A single publish job uploads everything at once, so a failed platform never leaves a partial release.

| Asset | What |
|---|---|
| `pypilot-linux-x64.tar.gz`, `pypilot-darwin-x64.tar.gz`, `pypilot-darwin-arm64.tar.gz`, `pypilot-windows-x64.zip` | The engine |
| `SHA256SUMS` <Badge type="tip" text="next" /> | One `<sha256>  <asset>` line (two spaces) per archive, sorted by name |
| `pypilot-helper.cdx.json` <Badge type="tip" text="next" /> | CycloneDX 1.5 SBOM of the engine |

<Badge type="tip" text="next" /> Every archive, `SHA256SUMS` and the SBOM carry a GitHub **build-provenance attestation**, which ties the file to the workflow run and commit that built it. Release 1.2.2 predates these; they start with the next engine release.

## Verifying a download <Badge type="tip" text="next" />

From the next engine release on, you can check both that an archive is intact (`SHA256SUMS`) and that it was built by this repository's Release workflow (the attestation). Replace `vX.Y.Z` with the release.

**1. Download the archive and `SHA256SUMS`** from the [release page](https://github.com/pypilot-dev/pypilot/releases), or with the GitHub CLI:

```bash
gh release download vX.Y.Z -R pypilot-dev/pypilot -p 'pypilot-linux-x64.tar.gz' -p SHA256SUMS
```

**2. Check the hash.** `SHA256SUMS` has one `<64 lowercase hex>  <asset>` line (two spaces) per archive, sorted by name. `--ignore-missing` skips the archives you did not download:

::: code-group

```bash [Linux]
sha256sum -c SHA256SUMS --ignore-missing
# pypilot-linux-x64.tar.gz: OK
```

```bash [macOS]
shasum -a 256 -c SHA256SUMS --ignore-missing
```

```powershell [Windows]
$want = (Select-String -Path SHA256SUMS -Pattern 'pypilot-windows-x64.zip').Line.Split(' ')[0]
$got  = (Get-FileHash pypilot-windows-x64.zip -Algorithm SHA256).Hash.ToLower()
if ($got -eq $want) { 'OK' } else { 'MISMATCH' }
```

:::

**3. Check the provenance** with the GitHub CLI. This verifies the Sigstore-signed attestation the Release workflow made, and prints the workflow run and commit it ties the file to:

```bash
gh attestation verify pypilot-linux-x64.tar.gz -R pypilot-dev/pypilot
gh attestation verify SHA256SUMS -R pypilot-dev/pypilot
gh attestation verify pypilot-helper.cdx.json -R pypilot-dev/pypilot
```

A file that was changed after the build, or built anywhere else, fails this check. Engine 1.2.2 and older have neither `SHA256SUMS` nor attestations.

**Reproducing a build.** Releases build with `--locked` from a clean target directory, and the run's log records `rustc -Vv`. Check out the tag and build with that compiler version to reproduce a binary.

## What each wrapper downloads and checks

| Wrapper | Downloads | Verifies |
|---|---|---|
| Zed extension | The newest release; its manifest only allows downloads from `github.com/pypilot-dev/pypilot` | HTTPS |
| VS Code 1.2.1 | The latest release | HTTPS |
| VS Code, next release <Badge type="tip" text="next" /> | The engine version pinned in its `package.json` (`"pypilotEngine": { "version", "sha256": { … } }`); with `pypilot.engineUpdates: "latest"`, newer releases too | Every archive is hashed before extraction: against the pinned SHA-256, or the release's `SHA256SUMS` for other versions. A mismatch, or a release without `SHA256SUMS`, installs nothing. There is no switch for unverified engines; use `pypilot.helperPath` for your own build |
| pypilot-cli 1.2.2 | The release it pins (`DEFAULT_TAG = "v1.2.2"`) | HTTPS; `.tar.gz` archives are extracted with Python's `data` filter where available, so nothing lands outside the target folder |
| pypilot-cli, next release <Badge type="tip" text="next" /> | Usually nothing: the platform wheels for Linux x86-64 (glibc 2.34+), macOS and Windows x64 bundle the pinned engine. Elsewhere, the pinned release (`DEFAULT_TAG`); with `PYPILOT_ENGINE_VERSION`, that release | Every download is hashed before it is extracted or run: against `ENGINE_SHA256` for the pinned release, against the release's `SHA256SUMS` otherwise. A mismatch deletes the file and fails; a release without `SHA256SUMS` is refused unless `PYPILOT_ALLOW_UNVERIFIED=1`. The wheels are built from archives checked against `ENGINE_SHA256` |

After each engine release, the `ecosystem-bump` workflow opens a pull request in each wrapper repository that bumps its pin and hashes (`scripts/bump_engine_pins.py`, reading the release's `SHA256SUMS`). It needs an `ECOSYSTEM_TOKEN` secret with write access to those repositories; without it, it only logs the command to run by hand. Merging a pin PR does not release the wrapper; that stays a manual step.

## Supply chain

- `cargo deny` runs on every push, pull request and daily: RustSec advisories, licenses, banned crates (OpenSSL) and crates.io as the only source.
- A nightly job runs against the real PyPI, PyTorch, NVIDIA and conda-forge and opens a `data-drift` issue when upstream changed under the data tables.
- Release and CI builds use `--locked`, so they build exactly `Cargo.lock`.
- <Badge type="tip" text="next" /> The VS Code extension's CI and release run `bun audit` on the dependencies it bundles, and each GitHub release carries a CycloneDX SBOM (`pypilot-vscode-X.Y.Z.cdx.json`) of them. `pypilot-cli`'s CI and publish job run `pip-audit`, and its releases carry `pypilot-cli.cdx.json`, which lists the bundled engine archives with their hashes.

## Data tables

The data tables are JSON, not code. Installed engines download them from the repository's `main` branch every `data_refresh_days` (7 by default), validate that they parse, and otherwise keep the copy compiled into the binary. Changes to them go through pull requests like any code. Set `data_refresh_days = 0` to never fetch them.

## Offline use

1. Put an engine on the machine: unpack a release archive (verify it as above) or `cargo install --path helper --locked`, and place `pypilot` on `PATH`. Both editor extensions and `pypilot-cli` use it instead of downloading.
2. Set `data_refresh_days = 0` in the global config file to stop table refreshes; see [Configuration](/reference/configuration).
3. Warm the PyPI metadata cache while online (any `pypilot doctor` does), or accept that unknown packages cannot be judged offline. Released versions are cached forever; "newest release" lookups for 24 hours.
4. With `pypilot-cli`, <Badge type="tip" text="next" /> install a platform wheel (the engine is inside) and set `PYPILOT_OFFLINE=1` so the wrapper never tries a download. See [pypilot-cli offline](/python/#offline).

What you see offline: `pypilot doctor` lists packages it has no cached metadata for as `Could not resolve` with the lookup error, and they do not narrow the supported range. <Badge type="tip" text="next" /> In the editor those imports get no diagnostic, instead of a false "PyPI has no project named …". `pypilot update-data --check` exits `2`.

## Reporting a vulnerability

Please do not open a public issue. Contact the maintainer privately through the [GitHub profile](https://github.com/Abdullah-Masood-05) linked from the contributing guide.

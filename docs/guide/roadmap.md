# Roadmap

PyPilot answers from package metadata and local checks, with no AI service and no API keys. Ideas that keep that property are the easiest to accept.

## Merged, ships in the next release

Marked <Badge type="tip" text="next" /> across this site.

**Engine** (on `main` after 1.2.2)
- `pypilot explain [package] [--json]`: what blocks each Python, `requires_python` against wheels per platform.
- `--json` on every command with a shared envelope, `--markdown` on `doctor` and `check`. [Details](/engine/output)
- pixi: multi-environment doctor, `pixi.lock` staleness and platform-wheel checks, `pixi_environment` / `--pixi-env`, `package_manager = "pixi"`, the "pixi can help here" advisory, `migrate-to-pixi`, `migrate-conda --to pixi`, `install-pixi`. [Details](/pixi/)
- Editor settings over LSP (`initializationOptions`, `workspace/didChangeConfiguration`).
- Releases with `SHA256SUMS`, a CycloneDX SBOM and build-provenance attestations; automatic pin-bump pull requests to the wrappers. [Details](/guide/security#where-releases-come-from)

**VS Code** (on `master` after 1.2.1)
- A pinned, SHA-256-verified engine, `pypilot.engineUpdates` and `pypilot.helperPath`. [Details](/vscode/#a-pinned-verified-engine)
- Multi-root workspaces, `pypilot.diagnosticsScope`, the report panel with Copy as Markdown / Save as HTML, palette `when` clauses, a richer status bar tooltip.
- pixi: `pypilot.pixiEnvironment`, Install pixi, Initialize pixi project, the walkthrough.

## In progress

<!-- verify after merge: move landed items up -->

- Engine: `--dry-run` plans for `setup`, `fix`, `install` and `doctor --fix --dry-run`; `pypilot logs [--tail]`; `pypilot completions <shell>`; `update-data --check`; `--version` details; workspace-wide diagnostics on the engine side; a protocol reference at `helper/src/lsp/PROTOCOL.md`.
- pypilot-cli: platform wheels that bundle the engine, and SHA-256 verification of downloads.

## Shipped

| Version | Highlights |
|---|---|
| Engine 1.2.2 | Python 3.15 across wheels, `requires-python`, interpreter discovery and recommendations |
| Engine 1.2.1 | The environment stays on the Python PyPilot chose (uv pins); `uv init --bare`; `check` accepts requirements like `mediapipe==0.10.14`; clearer "not on PyPI" message |
| Engine 1.2.0 | pixi support: detection, `pixi install` / `pixi add`, the pixi environment for diagnostics, pixi error explanations; PEP 440 fixes |
| Engine 1.1.2 | Moved to the `pypilot-dev` organization |

Full notes: [engine changelog](https://github.com/pypilot-dev/pypilot/blob/main/CHANGELOG.md), [VS Code changelog](https://github.com/pypilot-dev/pypilot-vscode/blob/master/CHANGELOG.md).

## Suggest something

Open a [feature request](https://github.com/pypilot-dev/pypilot/issues/new?template=feature_request.md). Describe the problem first, then the change you have in mind.

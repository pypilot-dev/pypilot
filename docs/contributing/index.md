# Contributing

PyPilot is free and open source, maintained by [Abdullah Masood](https://github.com/Abdullah-Masood-05). Bug reports, ideas and pull requests are all welcome. By taking part you agree to the [Code of Conduct](https://github.com/pypilot-dev/pypilot/blob/main/CODE_OF_CONDUCT.md).

## Which repository

| Repository | What lives there | Contributing guide |
|---|---|---|
| [pypilot](https://github.com/pypilot-dev/pypilot) | The engine (`helper/`), the Zed extension (`extension/`), the data tables, and this site (`docs/`) | [CONTRIBUTING.md](https://github.com/pypilot-dev/pypilot/blob/main/CONTRIBUTING.md) |
| [pypilot-vscode](https://github.com/pypilot-dev/pypilot-vscode) | The VS Code extension | [CONTRIBUTING.md](https://github.com/pypilot-dev/pypilot-vscode/blob/master/CONTRIBUTING.md) |
| [pypilot-pip](https://github.com/pypilot-dev/pypilot-pip) | `pypilot-cli` on PyPI | [CONTRIBUTING.md](https://github.com/pypilot-dev/pypilot-pip/blob/master/CONTRIBUTING.md) |

What PyPilot *decides* (which Python, which CUDA build, which imports are flagged) belongs in `pypilot`; how it shows up in VS Code in `pypilot-vscode`; installing through pip in `pypilot-pip`. Not sure? Open it in `pypilot`. For a bug, see [Filing an issue](/guide/troubleshooting#filing-an-issue).

## Conventions

Each repository has an `AGENTS.md` with the rules for humans and AI agents alike ([engine](https://github.com/pypilot-dev/pypilot/blob/main/AGENTS.md)). The essentials:

- **Small, logical commits** with conventional prefixes (`feat(scope):`, `fix:`, `test:`, `docs:`, `ci:`, `chore:`, `data:`) and a body that explains *why*.
- **No AI or tool trailers** in commits or pull requests.
- **LF line endings.**
- Contributors do not bump versions, tag or release; the maintainer does.

## The engine

```bash
git clone https://github.com/pypilot-dev/pypilot.git
cd pypilot
```

Every change to Rust must pass what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy -p pypilot-helper --all-targets -- -D warnings
cargo test -p pypilot-helper
cargo build -p pypilot-zed --target wasm32-wasip2 --release
```

Tests are offline and use recorded PyPI responses. More in [Building from source](/engine/building).

### Licenses

| Path | License |
|---|---|
| `helper/` | AGPL-3.0-or-later |
| `extension/` | Apache-2.0 (Zed's registry requires a permissive license for the code it ships) |

Your contribution is licensed under the license of the directory you change. Do not copy code from `helper/` into `extension/`.

### Data tables

The tables in `helper/data/` are compiled into the engine and re-downloaded from `main` by installed engines, so a change there reaches users within a week, and a broken table breaks engines in the field. Before any code change:

```bash
python scripts/check_data.py      # 0 current, 1 stale or schema break, 2 network error
```

If it reports anything stale, update the tables first, in their own `data: …` commit. Keep the schema backward compatible: only append rows or change values; never rename, remove or retype a field. See [Data tables](/reference/data-tables).

## The VS Code extension

Built with Bun:

```bash
bun install
bun run typecheck
bun run build
bun run package      # makes the .vsix
```

## The pip package

```bash
uv run python -m pytest -W error
```

CI runs it on Python 3.8 to 3.15 on Linux, Windows and macOS.

## This site

The docs live in `docs/` of the engine repo and are built with [VitePress](https://vitepress.dev) and Bun:

```bash
cd docs
bun install
bun run docs:dev        # http://localhost:5173/pypilot/
bun run docs:build      # fails on dead links
bun run docs:preview
```

- Pages are Markdown; the sidebar and nav are in `docs/.vitepress/config.mts`.
- Terminal output on the site is real: transcripts live in `docs/snippets/*.txt` and are shown either with `<<< @/snippets/<name>.txt{text}` or animated with `<TerminalDemo src="<name>" cmd="pypilot …" />`. When the engine's output changes, re-run the command and replace the file (shorten paths to `C:\src\…` or `~/src/…`).
- The theme (`docs/.vitepress/theme/`) is plain CSS and three small Vue components; it has no runtime dependencies besides VitePress and two self-hosted fonts. Animations respect `prefers-reduced-motion`.
- Pushing to `main` with changes under `docs/` deploys the site to GitHub Pages.

## Pull requests

1. Open an issue first for anything bigger than a small fix.
2. Fork, branch from the default branch, keep the change focused.
3. Run the checks above.
4. Describe what changed and why, link the issue, and mention anything you could not test (a platform, a GPU).

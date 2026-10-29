# The engine

`pypilot` is a single native binary, written in Rust, that is both a command-line tool and a language server. The Zed extension, the VS Code extension and the `pypilot-cli` package all run it; nothing else holds logic.

```text
pypilot <MODE> [ARG] [--path <dir>]
```

| Mode | Changes things? | Does |
|---|---|---|
| [`doctor`](./commands#doctor) | no | Environment and compatibility report (the default mode) |
| [`check <package>`](./commands#check) | no | Can this package run on this project's Python? |
| [`explain [package]`](./commands#explain) <Badge type="tip" text="next" /> | no | Why these Pythons: `requires_python` vs wheels per platform |
| [`setup`](./commands#setup) | yes | Build the environment on the recommended Python |
| [`install <package>`](./commands#install) | yes | Install one package and record it in the manifest |
| [`fix python`](./commands#fix-python) | yes | Rebuild the environment on the right Python |
| [`fix cuda`](./commands#fix-cuda) | yes | Re-pin torch/tensorflow to the build for your driver |
| [`update-data`](./commands#update-data) | cache only | Refresh the bundled data tables now |
| [`migrate-conda`](./commands#migrate-conda) | writes a file | `environment.yml` to `pyproject.toml` (or `pixi.toml` <Badge type="tip" text="next" />) |
| [`migrate-to-pixi`](./commands#migrate-to-pixi) <Badge type="tip" text="next" /> | with `--write` | Preview or write a `pixi.toml` |
| [`install-pixi`](./commands#install-pixi) <Badge type="tip" text="next" /> | installs pixi | Run pixi's official installer |
| [`lsp`](./commands#lsp) | — | Language server over stdio |

Modes marked next are on `main` and ship in the next release; release 1.2.2 has the others. `doctor`, `check` and `explain` only read. `setup`, `fix` and `install` share their code with the matching editor actions, so the terminal and the editor cannot disagree.

<TerminalDemo src="doctor-gpu" cmd="pypilot doctor" title="~/src/gpu" />

## Everyday use

```bash
pypilot                      # same as pypilot doctor
pypilot doctor --path ../api # another folder
pypilot check torch          # will torch run here?
pypilot check "numpy<2"      # requirements work too
pypilot explain              # what blocks each Python in this project
pypilot setup                # build .venv (or the pixi env) on the right Python
pypilot install httpx        # add a dependency through uv, pip or pixi
```

The full reference with every flag is on [Command reference](./commands). For scripting, see [Machine-readable output](./output).

## Where things live

| What | Where |
|---|---|
| Global config | `config.toml` in the platform config directory (for example `~/.config/pypilot/config.toml` on Linux) |
| Project config | `.zed/pypilot.toml`, else `pypilot.toml`, in the project root |
| PyPI metadata cache, refreshed data tables | the platform cache directory, under `pypilot` |

See [Configuration](/reference/configuration).

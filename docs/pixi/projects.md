# PyPilot in pixi projects

## Detection

A project is a pixi project when it has:

- a `pixi.toml`, or
- a `pyproject.toml` with `[tool.pixi.workspace]`.

pixi prefers `pixi.toml` when both exist, and so does PyPilot. Detection overrides [`package_manager`](/reference/configuration#package-manager): the environment of a pixi project belongs to pixi, and a `.venv` or `uv.lock` beside it would be a second, competing environment.

## The environment

| | Where it comes from |
|---|---|
| Environment | `.pixi/envs/<name>`, `<name>` = `--pixi-env`, else the `pixi_environment` setting (VS Code: `pypilot.pixiEnvironment`), else `default` |
| Python | the interpreter in that environment |
| Installed packages | conda and PyPI packages in that environment (conda packages count as installed for import diagnostics) |
| GPU checks | the torch/tensorflow builds in that environment |
| Recommended Python | the newest the dependencies support *within* the manifest's `python` spec |

If the selected environment does not exist in the manifest, doctor says so, names the environments that do, and falls back to `default`.

## Commands, translated

| You run | PyPilot runs |
|---|---|
| `pypilot setup`, **Fix environment** | `pixi install` |
| `pypilot install httpx`, an import quick fix | `pixi add --pypi httpx` |
| `pypilot fix python`, the "recreate on Python X.Y" quick fix | `pixi add python=X.Y` |
| `pypilot fix cuda` | nothing: pixi projects are never re-pinned to a PyTorch index |

<<< @/snippets/install-pixi-pkg.txt{text}

When `pixi install` fails, PyPilot turns the error into the same plain explanation it gives elsewhere: which package has no wheel for the environment's Python, and the fix, for example `pixi add python=3.11`.

## What doctor checks

In release 1.2.2 doctor reports an environment that is not installed yet or has no Python. The checks below marked <Badge type="tip" text="next" /> ship in the next release.

The **pixi** section lists the target platforms, the lock status, and every environment with its features, Python and dependencies (conda and PyPI separately).

- <Badge type="tip" text="next" /> **Every environment.** An environment is its features plus the default feature (unless it says `no-default-feature = true`); PyPilot merges them the way pixi does, so `python = ">=3.11"` at the top and `python = "3.11.*"` in a feature give 3.11, and `3.12.*` with `3.11.*` cannot be solved. Each environment's PyPI dependencies are checked against its Python.
- <Badge type="tip" text="next" /> **Different Pythons.** Environments on different Python versions get an Info finding, in case that was not a deliberate test matrix.
- <Badge type="tip" text="next" /> **The lock.** `pixi lock --check --dry-run` (which writes nothing) tells whether `pixi.lock` still matches the manifest. A stale lock gets a warning with the changes re-locking would make; a manifest that no longer solves gets an error with pixi's reason. A missing lock gets an Info finding.
- <Badge type="tip" text="next" /> **Every platform.** pixi solves the lock for every platform in `[workspace] platforms`, so a PyPI dependency with no wheel for one of them breaks `pixi lock` and `pixi install` on every machine, including yours. PyPilot catches that from PyPI metadata before pixi does.
- **Not installed yet.** An environment that was never installed gets a warning pointing at `pixi install`.

A project with a second environment pinned to 3.12, after editing the manifest:

<<< @/snippets/doctor-pixi-multi.txt{text}

## Choosing the environment <Badge type="tip" text="next" />

```toml
# pypilot.toml
pixi_environment = "cuda"
```

```bash
pypilot doctor --pixi-env cuda      # for one run
```

In VS Code, set `pypilot.pixiEnvironment`; the interpreter switches to `.pixi/envs/<name>` too. Install a non-default environment with `pixi install -e <name>`.

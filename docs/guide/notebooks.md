# Notebooks

## Diagnostics in cells

In VS Code each Jupyter notebook cell reaches the language server as its own document (`vscode-notebook-cell:` URIs). PyPilot checks Python cells like files: missing imports, imports that cannot run on the environment's Python, and removed attributes, with the same quick fixes. Markdown cells are skipped, and IPython magics and shell escapes (`%pip install …`, `!ls`) are masked out before analysis.

Quick fixes work across the notebook: select several imports to install them in one step, or use **Install all N missing packages in this notebook**.

Untitled notebooks are checked too; a saved notebook additionally lets PyPilot find local modules next to it.

## A kernel that starts

A notebook can only run in an environment that has `ipykernel`. In projects with notebooks, whenever PyPilot sets up the environment or installs a package, it makes sure `.venv` has:

```text
ipykernel>=6.31.0,<7
```

and warns when it is missing. In uv projects it is added as a dev dependency.

Why this range:

- **`<7`**: on ipykernel 7.x (reported on 7.1.0 and 7.2.0) VS Code notebooks can hang with cells stuck "pending", while 6.31.0 runs them. The maintainers' advice is to pin below 7 ([vscode-jupyter#17228](https://github.com/microsoft/vscode-jupyter/issues/17228)).
- **`>=6.31.0`**: the newest 6.x, released after Python 3.14 and tested on it. The 3.14 "kernel won't connect" failure was specific to 7.0.0 ([ipykernel#1445](https://github.com/ipython/ipykernel/issues/1445)). The floor also stops a cached old release from resolving.

If you install `ipykernel` yourself, your requirement is used instead.

## Selecting the kernel

With `pypilot.autoSelectVenv` (on by default) the VS Code extension makes the project environment the active interpreter, which is also what the Jupyter kernel picker recommends. In pixi projects that is `.pixi/envs/<name>`. **PyPilot: Use Project Environment (.venv / pixi)** does it on demand.

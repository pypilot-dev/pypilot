# Other editors

PyPilot's language server is editor-agnostic: any client that speaks LSP over stdio can run `pypilot lsp`. You get import diagnostics, quick fixes, and the workspace notification as `window/showMessageRequest`. Run it **alongside** your usual Python server (pyright, basedpyright, pylsp, ruff, ty); it does not do completion, hover or type checking.

First put the engine on your `PATH`: `pip install pypilot-cli`, a release archive, or `cargo install --path helper --locked` (see [Getting started](/guide/getting-started)). Check with `pypilot --version`.

The snippets below are the ones in [`helper/src/lsp/PROTOCOL.md`](https://github.com/pypilot-dev/pypilot/blob/main/helper/src/lsp/PROTOCOL.md) <Badge type="tip" text="next" />, the protocol reference kept next to the server code. They turn on [workspace diagnostics](/reference/configuration#diagnostics-scope) as an example of passing a setting; leave that out to check open files only (engine 1.2.2 ignores it anyway). The commands from [Engine CLI](/engine/) work in any terminal next to them.

::: tip How settings reach PyPilot
PyPilot never asks the editor for settings (`workspace/configuration`); the client must push them, in `initializationOptions` or `workspace/didChangeConfiguration`. Each message **replaces** the previous editor settings, so pick one way per editor, as below. `pypilot.toml` in the project always works and wins over the editor. See [precedence](./configuration#precedence).
:::

## Neovim

Neovim 0.11+ with `vim.lsp.config`:

```lua
-- init.lua
vim.lsp.config('pypilot', {
  cmd = { 'pypilot', 'lsp' },
  filetypes = { 'python' },
  root_markers = { 'pyproject.toml', 'requirements.txt', 'pixi.toml', 'setup.py', '.git' },
  init_options = { diagnosticsScope = 'workspace' },
})
vim.lsp.enable('pypilot')
```

Use `init_options`, not `settings`: Neovim serves `settings` through `workspace/configuration`, which PyPilot never requests. Quick fixes are under `vim.lsp.buf.code_action()` (`gra` by default in 0.11). The notification buttons appear through `vim.ui.select`.

## Helix

```toml
# ~/.config/helix/languages.toml
[language-server.pypilot]
command = "pypilot"
args = ["lsp"]
config = { diagnosticsScope = "workspace" }

[[language]]
name = "python"
# List the servers you already use as well; this array replaces the default.
language-servers = ["pyright", "pypilot"]
```

Code actions: `space a`. Helix shows `window/showMessage` in the status line.

## Emacs (Eglot)

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '((python-mode python-ts-mode) . ("pypilot" "lsp"))))

;; Eglot sends this with workspace/didChangeConfiguration on connect, which
;; replaces initializationOptions (see the tip above), so configure it here.
(setq-default eglot-workspace-configuration
              '(:pypilot (:diagnosticsScope "workspace")))
```

Eglot runs one server per buffer, so this entry replaces your other Python server for those modes; put the two behind an LSP multiplexer if you want both.

### lsp-mode

`lsp-mode` can run PyPilot as an add-on client next to pyright. This snippet is not in PROTOCOL.md; adapt it to your setup:

```elisp
(with-eval-after-load 'lsp-mode
  (lsp-register-client
   (make-lsp-client :new-connection (lsp-stdio-connection '("pypilot" "lsp"))
                    :activation-fn (lsp-activate-on "python")
                    :add-on? t
                    :server-id 'pypilot)))
```

## What to expect

- The project root is the client's first workspace folder (or `rootUri`). Open the project folder, not a single file.
- `pypilot.showDetails` opens the report with `window/showDocument`; clients that do not support it get the report as a message.
- With `diagnosticsScope = "workspace"`, the server watches `**/*.py` and `**/*.ipynb` only if your client supports dynamic registration of `workspace/didChangeWatchedFiles`. Without it, files changed outside the editor are re-checked after the next install, rebuild, terminal command or configuration change.
- A terminal command (`pypilot setup`, `pypilot doctor`, …) run in the project makes the running server re-check, as in Zed.
- Everything is described in the [LSP protocol](./lsp) reference.

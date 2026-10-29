# Other editors

PyPilot's language server is editor-agnostic: any client that speaks LSP over stdio can run `pypilot lsp`. You get import diagnostics, quick fixes, and the workspace notification as `window/showMessageRequest`. Run it **alongside** your usual Python server (pyright, basedpyright, pylsp, ruff); it does not do completion, hover or type checking.

First put the engine on your `PATH`: `pip install pypilot-cli`, a release archive, or `cargo install --path helper --locked` (see [Getting started](/guide/getting-started)). Check with `pypilot --version`.

The configurations below follow each editor's documented way of adding a language server; adapt them to your setup. The commands from [Engine CLI](/engine/) work in any terminal next to them.

## Neovim

With Neovim 0.11+ built-in configuration:

```lua
-- init.lua
vim.lsp.config('pypilot', {
  cmd = { 'pypilot', 'lsp' },
  filetypes = { 'python' },
  root_markers = { 'pyproject.toml', 'requirements.txt', 'pixi.toml', 'environment.yml', '.git' },
  -- Optional: the same keys as pypilot.toml, in camelCase.
  init_options = { notifications = 'problems-only' },
})
vim.lsp.enable('pypilot')
```

Quick fixes are under `vim.lsp.buf.code_action()` (`gra` by default in 0.11). The notification buttons appear through `vim.ui.select`.

## Helix

```toml
# ~/.config/helix/languages.toml
[language-server.pypilot]
command = "pypilot"
args = ["lsp"]

[[language]]
name = "python"
language-servers = ["pyright", "pypilot"]   # keep your usual server first
```

Code actions: `space a`. Helix shows `window/showMessage` in the status line.

## Emacs (eglot)

eglot runs one server per major mode, so in eglot PyPilot replaces your Python server unless you put an LSP multiplexer in front. As the only server:

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '((python-mode python-ts-mode) . ("pypilot" "lsp"))))
```

With `lsp-mode`, register it as an add-on client so it runs next to pyright:

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
- Settings come from `pypilot.toml` (and the global config); editors that send `initializationOptions` can override them per session. See [Configuration](./configuration).
- `pypilot.showDetails` opens the report with `window/showDocument`; clients that do not support it get the report as a message.
- Everything is described in the [LSP protocol](./lsp) reference.

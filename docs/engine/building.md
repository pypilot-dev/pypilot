# Building from source

You need the stable Rust toolchain. The repository's `rust-toolchain.toml` pins `stable` with `rustfmt`, `clippy` and the `wasm32-wasip2` target, so `rustup` installs them the first time you run `cargo` in the repo.

```bash
git clone https://github.com/pypilot-dev/pypilot.git
cd pypilot

# the engine
cargo build -p pypilot-helper --release      # target/release/pypilot
cargo install --path helper --locked         # or put it on PATH

# the Zed extension (WebAssembly)
cargo build -p pypilot-zed --target wasm32-wasip2 --release
```

Both editor extensions prefer a `pypilot` on `PATH` over a downloaded release, so `cargo install` is also how you try a change in your editor. After replacing the binary, restart the language server: in VS Code run **Developer: Reload Window**; in Zed, restart the language server or reopen the folder.

::: tip Windows
A running editor locks `pypilot.exe`. Rename the old one (for example to `pypilot.exe.old`), run `cargo install --path helper --locked`, reload the editor, then delete the old file.
:::

## Tests

```bash
cargo test -p pypilot-helper
```

Tests run offline against recorded PyPI responses and fixed Linux x86-64 target tags. The checks CI runs, and that every pull request touching Rust must pass:

```bash
cargo fmt --all -- --check
cargo clippy -p pypilot-helper --all-targets -- -D warnings
cargo test -p pypilot-helper
cargo build -p pypilot-zed --target wasm32-wasip2 --release
```

## Repository layout

```text
pypilot/
├── extension/           Zed WebAssembly shim (extension.toml, src/lib.rs)
├── helper/              The engine
│   ├── src/
│   │   ├── main.rs      Mode dispatch
│   │   ├── cli/         Subcommands
│   │   ├── lsp/         Language server
│   │   ├── core/        Environments, uv/pip/pixi drivers, solver
│   │   ├── pypi/        Metadata client, wheel tags, cache
│   │   └── matrix/      Driver/framework/conda-forge tables
│   ├── data/            Bundled JSON tables
│   └── tests/           Fixtures and offline tests
├── tasks/               Zed task templates
├── docs/                This site
└── scripts/             check_data.py and table generators
```

## Licenses

`helper/` (the engine) is AGPL-3.0-or-later. `extension/` (the Zed shim) is Apache-2.0, as Zed's registry requires for the code it ships. A contribution is licensed under the license of the directory it changes.

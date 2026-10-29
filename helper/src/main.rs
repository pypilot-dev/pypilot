// Copyright (C) 2026 PyPilot contributors
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the license for
// details: <https://www.gnu.org/licenses/>.

//! PyPilot helper entrypoint — mode dispatch.
//!
//! `pypilot <mode>`:
//!   * `doctor` — read-only probe report (nothing is executed/installed).
//!   * `setup`  — full F1 bootstrap, honoring the `package_manager` setting.
//!   * `lsp`    — stdio LSP server (the mode Zed launches).
//!   * `check`, `explain`, `install`, `fix`, `update-data`, `migrate-conda` — see `--help`.
//!
//! Arg parsing is hand-rolled (no shell involvement anywhere) so every downstream
//! spawned command uses arg vectors, never a shell string.

use std::process::ExitCode;

use pypilot::{cli, lsp};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "doctor".to_string());
    let rest: Vec<String> = args.collect();

    match mode.as_str() {
        "lsp" => {
            // The LSP server owns its own multi-threaded runtime.
            lsp::run_stdio();
            ExitCode::SUCCESS
        }
        "doctor" | "setup" | "check" | "explain" | "fix" | "install" | "update-data"
        | "migrate-conda" => {
            // Consoles that predate Windows Terminal print ANSI escapes as
            // literal text unless the process opts in to VT processing.
            #[cfg(windows)]
            let _ = colored::control::set_virtual_terminal(true);

            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("pypilot: failed to start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let code = rt.block_on(cli::dispatch(&mode, &rest));
            match code {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("pypilot {mode}: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        "--version" | "-V" | "version" => {
            println!("pypilot {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" => {
            print_help();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("pypilot: unknown mode `{other}`\n");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "pypilot {ver}, a deterministic Python environment doctor\n\n\
         USAGE:\n    pypilot <MODE> [ARG] [--path <dir>]\n\n\
         MODES:\n\
         \x20   doctor            Read-only environment and compatibility report\n\
         \x20   setup             Build the environment (uv by default, pip if configured)\n\
         \x20   check <package>   Can this package run on this project's Python?\n\
         \x20   explain [package] Why these Pythons: requires_python vs wheels per platform;\n\
         \x20                     without a package, what blocks each Python in the project\n\
         \x20   install <package> Install one package and record it in the manifest\n\
         \x20   fix python        Rebuild the environment on the right Python version\n\
         \x20   fix cuda          Re-pin torch/tensorflow to the build matching the driver\n\
         \x20   update-data       Force-refresh the bundled driver/framework/import tables\n\
         \x20   migrate-conda     Translate environment.yml into a pyproject.toml\n\
         \x20   lsp               Run as an LSP server over stdio, used by the Zed extension\n\n\
         OPTIONS:\n\
         \x20   --path <dir>       Workspace directory to analyze (default: current dir)\n\
         \x20   --json             explain: print the explanation as JSON\n\
         \x20   --pixi-env <name>  pixi environment to use (default: the pixi_environment setting)\n",
        ver = env!("CARGO_PKG_VERSION")
    );
}

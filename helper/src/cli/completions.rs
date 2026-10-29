//! `pypilot completions <bash|zsh|fish|powershell>`: shell completion scripts.
//!
//! Argument parsing is hand-rolled (no clap), so there is nothing to derive
//! completions from. Instead [`COMMANDS`] lists every mode and flag once, and
//! each shell's script is generated from it; adding a flag to the table adds it
//! to all four shells. A test checks the table against `--help`.

/// What a flag or positional argument takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val {
    /// Nothing (a switch, or a command without a positional).
    None,
    /// A directory.
    Dir,
    /// Free text: offer nothing.
    Text,
    /// An optional number after a switch (`--tail [N]`): offer nothing, and do
    /// not swallow the next flag.
    OptNumber,
    /// One of a fixed set.
    Choices(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy)]
pub struct Flag {
    pub long: &'static str,
    pub short: Option<&'static str>,
    pub value: Val,
    pub help: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct Cmd {
    pub name: &'static str,
    /// Other spellings that behave the same (`--version` for `version`).
    pub aliases: &'static [&'static str],
    pub help: &'static str,
    pub positional: Val,
    pub flags: &'static [Flag],
    /// Takes the project flags in [`GLOBAL`].
    pub project: bool,
}

const fn switch(long: &'static str, help: &'static str) -> Flag {
    Flag {
        long,
        short: None,
        value: Val::None,
        help,
    }
}

const fn takes(long: &'static str, value: Val, help: &'static str) -> Flag {
    Flag {
        long,
        short: None,
        value,
        help,
    }
}

const SHELLS: &[&str] = &["bash", "zsh", "fish", "powershell"];
const PIXI_PLATFORMS: &[&str] = &["linux-64", "linux-aarch64", "osx-64", "osx-arm64", "win-64"];

/// Flags every project command accepts.
pub const GLOBAL: &[Flag] = &[
    Flag {
        long: "--path",
        short: Some("-p"),
        value: Val::Dir,
        help: "Workspace directory to analyze",
    },
    takes("--pixi-env", Val::Text, "pixi environment to use"),
    Flag {
        long: "--help",
        short: Some("-h"),
        value: Val::None,
        help: "Show help",
    },
];

const JSON: Flag = switch("--json", "Print the result as one JSON document");
const MARKDOWN: Flag = switch("--markdown", "Print the Markdown report");
const DRY_RUN: Flag = switch("--dry-run", "Show the steps without running them");

const PIXI_WRITE: Flag = switch(
    "--write",
    "Write the pixi manifest instead of previewing it",
);
const PIXI_INSTALL: Flag = switch("--install", "Run pixi install after writing");
const PIXI_PLATFORM: Flag = takes(
    "--platform",
    Val::Choices(PIXI_PLATFORMS),
    "Add a target platform",
);

/// Every mode and its flags. Keep in step with `print_help` in `main.rs`.
pub const COMMANDS: &[Cmd] = &[
    Cmd {
        name: "doctor",
        aliases: &[],
        help: "Read-only environment and compatibility report",
        positional: Val::None,
        flags: &[
            JSON,
            MARKDOWN,
            switch("--fix", "Apply the suggested fixes"),
            switch("--dry-run", "Show what --fix would run"),
        ],
        project: true,
    },
    Cmd {
        name: "setup",
        aliases: &[],
        help: "Build the environment",
        positional: Val::None,
        flags: &[DRY_RUN, JSON],
        project: true,
    },
    Cmd {
        name: "check",
        aliases: &[],
        help: "Can this package run on this project's Python",
        positional: Val::Text,
        flags: &[
            switch(
                "--suggest-alternatives",
                "Show conda-forge alternatives even when it passes",
            ),
            JSON,
            MARKDOWN,
        ],
        project: true,
    },
    Cmd {
        name: "explain",
        aliases: &[],
        help: "Why these Pythons",
        positional: Val::Text,
        flags: &[JSON],
        project: true,
    },
    Cmd {
        name: "install",
        aliases: &[],
        help: "Install one package and record it in the manifest",
        positional: Val::Text,
        flags: &[DRY_RUN, JSON],
        project: true,
    },
    Cmd {
        name: "fix",
        aliases: &[],
        help: "Rebuild on the right Python, or re-pin the CUDA build",
        positional: Val::Choices(&["python", "cuda"]),
        flags: &[DRY_RUN, JSON],
        project: true,
    },
    Cmd {
        name: "update-data",
        aliases: &[],
        help: "Refresh the data tables",
        positional: Val::None,
        flags: &[switch("--check", "Report staleness without writing"), JSON],
        project: false,
    },
    Cmd {
        name: "migrate-conda",
        aliases: &[],
        help: "Translate environment.yml",
        positional: Val::None,
        flags: &[
            takes(
                "--to",
                Val::Choices(&["pixi", "pyproject"]),
                "Target manifest",
            ),
            PIXI_WRITE,
            PIXI_INSTALL,
            PIXI_PLATFORM,
            JSON,
        ],
        project: true,
    },
    Cmd {
        name: "migrate-to-pixi",
        aliases: &[],
        help: "Preview or write a pixi.toml",
        positional: Val::None,
        flags: &[PIXI_WRITE, PIXI_INSTALL, PIXI_PLATFORM, JSON],
        project: true,
    },
    Cmd {
        name: "install-pixi",
        aliases: &[],
        help: "Install pixi with its official installer",
        positional: Val::None,
        flags: &[JSON],
        project: false,
    },
    Cmd {
        name: "logs",
        aliases: &[],
        help: "Print the log path, or the last entries",
        positional: Val::None,
        flags: &[
            takes("--tail", Val::OptNumber, "Show the last N entries"),
            switch("--json", "Print raw JSON lines"),
        ],
        project: false,
    },
    Cmd {
        name: "completions",
        aliases: &[],
        help: "Print a shell completion script",
        positional: Val::Choices(SHELLS),
        flags: &[],
        project: false,
    },
    Cmd {
        name: "version",
        aliases: &["--version", "-V"],
        help: "Engine version, commit, target and data tables",
        positional: Val::None,
        flags: &[
            JSON,
            switch("--verbose", "Include commit, target and data tables"),
        ],
        project: false,
    },
    Cmd {
        name: "lsp",
        aliases: &[],
        help: "Run as an LSP server over stdio",
        positional: Val::None,
        flags: &[],
        project: false,
    },
    Cmd {
        name: "help",
        aliases: &["--help", "-h"],
        help: "Show help",
        positional: Val::None,
        flags: &[],
        project: false,
    },
];

/// Run `pypilot completions <shell>`.
pub fn run(args: &[String]) -> Result<(), String> {
    let shell = args.iter().find(|a| !a.starts_with('-'));
    match shell.map(String::as_str) {
        Some(shell) => match script(shell) {
            Some(text) => {
                print!("{text}");
                Ok(())
            }
            None => Err(format!(
                "unknown shell `{shell}`; use one of: {}",
                SHELLS.join(", ")
            )),
        },
        None => Err(format!("usage: pypilot completions <{}>", SHELLS.join("|"))),
    }
}

/// The completion script for `shell`.
pub fn script(shell: &str) -> Option<String> {
    match shell {
        "bash" => Some(bash()),
        "zsh" => Some(zsh()),
        "fish" => Some(fish()),
        "powershell" | "pwsh" => Some(powershell()),
        _ => None,
    }
}

fn flags_of(cmd: &Cmd) -> Vec<Flag> {
    let mut all: Vec<Flag> = cmd.flags.to_vec();
    if cmd.project {
        all.extend_from_slice(GLOBAL);
    }
    all
}

fn top_level_words() -> Vec<&'static str> {
    COMMANDS
        .iter()
        .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
        .collect()
}

// --- bash --------------------------------------------------------------------

fn bash() -> String {
    let mut s = String::from(
        "# pypilot bash completion. Load with:\n\
         #   source <(pypilot completions bash)\n\
         _pypilot() {\n\
         \x20   local cur prev cmd opts words\n\
         \x20   COMPREPLY=()\n\
         \x20   cur=\"${COMP_WORDS[COMP_CWORD]}\"\n\
         \x20   prev=\"${COMP_WORDS[COMP_CWORD-1]}\"\n\
         \x20   if [[ $COMP_CWORD -eq 1 ]]; then\n",
    );
    s.push_str(&format!(
        "        COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )\n        return 0\n    fi\n",
        top_level_words().join(" ")
    ));
    s.push_str("    cmd=\"${COMP_WORDS[1]}\"\n    case \"$cmd\" in\n");
    for cmd in COMMANDS {
        let flags = flags_of(cmd);
        let names: Vec<&str> = std::iter::once(cmd.name)
            .chain(cmd.aliases.iter().copied())
            .collect();
        s.push_str(&format!("        {})\n", names.join("|")));
        s.push_str("            case \"$prev\" in\n");
        for f in &flags {
            let pat = match f.short {
                Some(short) => format!("{}|{short}", f.long),
                None => f.long.to_string(),
            };
            match f.value {
                Val::Dir => s.push_str(&format!(
                    "                {pat}) COMPREPLY=( $(compgen -d -- \"$cur\") ); return 0 ;;\n"
                )),
                Val::Text => s.push_str(&format!("                {pat}) return 0 ;;\n")),
                Val::Choices(c) => s.push_str(&format!(
                    "                {pat}) COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ); return 0 ;;\n",
                    c.join(" ")
                )),
                Val::None | Val::OptNumber => {}
            }
        }
        s.push_str("            esac\n");
        let opts: Vec<&str> = flags
            .iter()
            .flat_map(|f| std::iter::once(f.long).chain(f.short))
            .collect();
        let words = match cmd.positional {
            Val::Choices(c) => c.join(" "),
            _ => String::new(),
        };
        s.push_str(&format!(
            "            opts=\"{}\"\n            words=\"{words}\"\n            ;;\n",
            opts.join(" ")
        ));
    }
    s.push_str(
        "        *) return 0 ;;\n\
         \x20   esac\n\
         \x20   if [[ \"$cur\" == -* || -z \"$words\" ]]; then\n\
         \x20       COMPREPLY=( $(compgen -W \"$opts\" -- \"$cur\") )\n\
         \x20   else\n\
         \x20       COMPREPLY=( $(compgen -W \"$words\" -- \"$cur\") )\n\
         \x20   fi\n\
         \x20   return 0\n\
         }\n\
         complete -F _pypilot pypilot\n",
    );
    s
}

// --- zsh ---------------------------------------------------------------------

fn zsh_escape(help: &str) -> String {
    help.replace('\'', "'\\''")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(':', "\\:")
}

fn zsh() -> String {
    let mut s = String::from(
        "#compdef pypilot\n\
         # pypilot zsh completion. Put this in a file named _pypilot on $fpath,\n\
         # or load it directly: source <(pypilot completions zsh)\n\
         _pypilot() {\n\
         \x20 local -a commands\n\
         \x20 commands=(\n",
    );
    for cmd in COMMANDS {
        s.push_str(&format!("    '{}:{}'\n", cmd.name, zsh_escape(cmd.help)));
    }
    s.push_str(
        "  )\n\
         \x20 if (( CURRENT == 2 )); then\n\
         \x20   _describe -t commands 'pypilot command' commands\n\
         \x20   compadd -- --version -V --help -h\n\
         \x20   return\n\
         \x20 fi\n\
         \x20 local cmd=$words[2]\n\
         \x20 shift words\n\
         \x20 (( CURRENT-- ))\n\
         \x20 case $cmd in\n",
    );
    for cmd in COMMANDS {
        let names: Vec<&str> = std::iter::once(cmd.name)
            .chain(cmd.aliases.iter().copied())
            .collect();
        if flags_of(cmd).is_empty() && cmd.positional == Val::None {
            // `_arguments` with no specs is an error; nothing to offer.
            s.push_str(&format!("    {})\n      ;;\n", names.join("|")));
            continue;
        }
        s.push_str(&format!("    {})\n      _arguments -s", names.join("|")));
        for f in flags_of(cmd) {
            let action = match f.value {
                Val::None | Val::OptNumber => String::new(),
                Val::Dir => ":directory:_files -/".to_string(),
                Val::Text => ":value: ".to_string(),
                Val::Choices(c) => format!(":value:({})", c.join(" ")),
            };
            let help = zsh_escape(f.help);
            match f.short {
                Some(short) => s.push_str(&format!(
                    " \\\n        '({long} {short})'{{{long},{short}}}'[{help}]{action}'",
                    long = f.long
                )),
                None => s.push_str(&format!(" \\\n        '{}[{help}]{action}'", f.long)),
            }
        }
        match cmd.positional {
            Val::Choices(c) => s.push_str(&format!(" \\\n        '1:argument:({})'", c.join(" "))),
            Val::Text => s.push_str(" \\\n        '::argument: '"),
            _ => {}
        }
        s.push_str("\n      ;;\n");
    }
    s.push_str(
        "  esac\n\
         }\n\
         if [ \"$funcstack[1]\" = \"_pypilot\" ]; then\n\
         \x20 _pypilot \"$@\"\n\
         else\n\
         \x20 compdef _pypilot pypilot\n\
         fi\n",
    );
    s
}

// --- fish --------------------------------------------------------------------

fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn fish() -> String {
    let mut s = String::from(
        "# pypilot fish completion. Load with:\n\
         #   pypilot completions fish > ~/.config/fish/completions/pypilot.fish\n\
         complete -c pypilot -f\n\
         complete -c pypilot -n __fish_use_subcommand -s V -l version -d 'Print the version'\n\
         complete -c pypilot -n __fish_use_subcommand -s h -l help -d 'Show help'\n",
    );
    for cmd in COMMANDS {
        s.push_str(&format!(
            "complete -c pypilot -n __fish_use_subcommand -a {} -d {}\n",
            cmd.name,
            fish_quote(cmd.help)
        ));
    }
    for cmd in COMMANDS {
        let cond = fish_quote(&format!("__fish_seen_subcommand_from {}", cmd.name));
        if let Val::Choices(c) = cmd.positional {
            s.push_str(&format!(
                "complete -c pypilot -n {cond} -a {}\n",
                fish_quote(&c.join(" "))
            ));
        }
        for f in flags_of(cmd) {
            let mut line = format!(
                "complete -c pypilot -n {cond} -l {}",
                f.long.trim_start_matches('-')
            );
            if let Some(short) = f.short {
                line.push_str(&format!(" -s {}", short.trim_start_matches('-')));
            }
            match f.value {
                Val::None | Val::OptNumber => {}
                Val::Dir => line.push_str(" -x -a '(__fish_complete_directories)'"),
                Val::Text => line.push_str(" -x"),
                Val::Choices(c) => line.push_str(&format!(" -x -a {}", fish_quote(&c.join(" ")))),
            }
            line.push_str(&format!(" -d {}\n", fish_quote(f.help)));
            s.push_str(&line);
        }
    }
    s
}

// --- PowerShell --------------------------------------------------------------

fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn ps_array(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| ps_quote(i)).collect();
    format!("@({})", quoted.join(", "))
}

fn powershell() -> String {
    let mut s = String::from(
        "# pypilot PowerShell completion. Load with:\n\
         #   pypilot completions powershell | Out-String | Invoke-Expression\n\
         Register-ArgumentCompleter -Native -CommandName 'pypilot', 'pypilot.exe' -ScriptBlock {\n\
         \x20   param($wordToComplete, $commandAst, $cursorPosition)\n\
         \x20   $commands = @{\n",
    );
    for cmd in COMMANDS {
        let flags = flags_of(cmd);
        let names: Vec<&str> = flags
            .iter()
            .flat_map(|f| std::iter::once(f.long).chain(f.short))
            .collect();
        let mut values = Vec::new();
        for f in &flags {
            let v = match f.value {
                Val::Choices(c) => ps_array(c),
                Val::Dir | Val::Text => "@()".to_string(),
                Val::None | Val::OptNumber => continue,
            };
            for name in std::iter::once(f.long).chain(f.short) {
                values.push(format!("{} = {v}", ps_quote(name)));
            }
        }
        let positional = match cmd.positional {
            Val::Choices(c) => ps_array(c),
            _ => "@()".to_string(),
        };
        let spec = format!(
            "@{{ Flags = {}; Takes = @{{ {} }}; Positional = {positional} }}",
            ps_array(&names),
            values.join("; ")
        );
        for name in std::iter::once(cmd.name).chain(cmd.aliases.iter().copied()) {
            s.push_str(&format!("        {} = {spec}\n", ps_quote(name)));
        }
    }
    s.push_str(&format!(
        "    }}\n\
         \x20   $top = {}\n",
        ps_array(&top_level_words())
    ));
    s.push_str(
        "    $tokens = @($commandAst.CommandElements |\n\
         \x20       Where-Object { $_.Extent.StartOffset -lt $cursorPosition } |\n\
         \x20       ForEach-Object { $_.Extent.Text })\n\
         \x20   if ($wordToComplete -ne '' -and $tokens.Count -gt 0) {\n\
         \x20       $tokens = @($tokens | Select-Object -First ($tokens.Count - 1))\n\
         \x20   }\n\
         \x20   $candidates = @()\n\
         \x20   if ($tokens.Count -le 1) {\n\
         \x20       $candidates = $top\n\
         \x20   } else {\n\
         \x20       $spec = $commands[$tokens[1]]\n\
         \x20       if ($null -eq $spec) { return }\n\
         \x20       $prev = $tokens[$tokens.Count - 1]\n\
         \x20       if ($tokens.Count -gt 2 -and $spec.Takes.ContainsKey($prev)) {\n\
         \x20           $candidates = $spec.Takes[$prev]\n\
         \x20           if ($candidates.Count -eq 0) { return }\n\
         \x20       } elseif ($wordToComplete.StartsWith('-') -or $spec.Positional.Count -eq 0) {\n\
         \x20           $candidates = $spec.Flags\n\
         \x20       } else {\n\
         \x20           $candidates = $spec.Positional\n\
         \x20       }\n\
         \x20   }\n\
         \x20   $candidates | Where-Object { $_ -like \"$wordToComplete*\" } | ForEach-Object {\n\
         \x20       [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)\n\
         \x20   }\n\
         }\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_has_a_script() {
        for shell in SHELLS {
            let text = script(shell).expect("script");
            for cmd in COMMANDS {
                assert!(text.contains(cmd.name), "{shell} misses {}", cmd.name);
            }
        }
        assert!(script("tcsh").is_none());
    }

    #[test]
    fn table_covers_the_requested_flags() {
        let has = |cmd: &str, flag: &str| {
            COMMANDS
                .iter()
                .find(|c| c.name == cmd)
                .is_some_and(|c| flags_of(c).iter().any(|f| f.long == flag))
        };
        for (cmd, flag) in [
            ("doctor", "--json"),
            ("doctor", "--markdown"),
            ("doctor", "--fix"),
            ("doctor", "--dry-run"),
            ("doctor", "--path"),
            ("doctor", "--pixi-env"),
            ("explain", "--json"),
            ("check", "--markdown"),
            ("setup", "--json"),
            ("install-pixi", "--json"),
            ("setup", "--dry-run"),
            ("fix", "--dry-run"),
            ("install", "--dry-run"),
            ("update-data", "--check"),
            ("migrate-conda", "--to"),
            ("migrate-to-pixi", "--write"),
            ("migrate-to-pixi", "--install"),
            ("migrate-to-pixi", "--platform"),
            ("logs", "--tail"),
        ] {
            assert!(has(cmd, flag), "{cmd} {flag}");
        }
    }

    #[test]
    fn every_mode_in_the_table_is_dispatched() {
        // The modes `main.rs` accepts; a new one must be added to the table.
        let main = include_str!("../main.rs");
        for cmd in COMMANDS {
            assert!(
                main.contains(&format!("\"{}\"", cmd.name)),
                "`{}` is not a mode in main.rs",
                cmd.name
            );
        }
    }

    /// `bash -n` on the generated script, where bash is the system shell.
    #[cfg(unix)]
    #[test]
    fn bash_script_parses() {
        use std::io::Write;
        let Ok(mut child) = std::process::Command::new("bash")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
        else {
            return; // no bash on this machine
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(bash().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn help_text_needs_no_escaping_surprises() {
        // zsh and fish quoting is covered by the escape helpers; make sure
        // they actually escape.
        assert_eq!(zsh_escape("a [b]: 'c'"), "a \\[b\\]\\: '\\''c'\\''");
        assert_eq!(fish_quote("it's"), "'it\\'s'");
        assert_eq!(ps_quote("it's"), "'it''s'");
    }
}

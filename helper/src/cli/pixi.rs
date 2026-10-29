//! `pypilot migrate-to-pixi`, `pypilot migrate-conda --to pixi` and
//! `pypilot install-pixi`, thin wrappers over [`crate::core::pixi_migrate`]
//! and [`crate::core::pixi_install`].

use std::path::Path;

use colored::Colorize;

use crate::core::pixi_install;
use crate::core::pixi_migrate::{self, MigrateOptions, MigrationSource, MigrationSummary};
use crate::settings::Settings;

/// Options shared by both migrations: `--write`, `--install`, and
/// `--platform <p>` (repeatable, or comma-separated).
pub fn options(args: &[String], source: MigrationSource) -> MigrateOptions {
    let mut opts = MigrateOptions {
        source,
        ..MigrateOptions::default()
    };
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--write" => opts.write = true,
            "--install" => opts.install = true,
            "--platform" | "--platforms" => {
                if let Some(p) = iter.next() {
                    opts.extra_platforms.push(p.clone());
                }
            }
            other => {
                if let Some(p) = other
                    .strip_prefix("--platform=")
                    .or_else(|| other.strip_prefix("--platforms="))
                {
                    opts.extra_platforms.push(p.to_string());
                }
            }
        }
    }
    opts
}

pub async fn migrate(
    workspace: &Path,
    settings: &Settings,
    opts: MigrateOptions,
) -> crate::Result<()> {
    if opts.install && !opts.write {
        anyhow::bail!(
            "--install needs --write: a dry run changes nothing, so there is nothing to install"
        );
    }
    let summary = pixi_migrate::migrate_to_pixi(workspace, settings, opts).await?;
    print_summary(&summary);
    if summary.written {
        // Let a running LSP pick up the new manifest immediately.
        crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Setup);
    }
    if summary.ok() {
        Ok(())
    } else {
        anyhow::bail!("pixi install did not succeed")
    }
}

fn print_summary(s: &MigrationSummary) {
    if !s.written {
        println!("{}", "Dry run: nothing was written.".bold());
        println!();
        for line in s.preview.lines() {
            if line.starts_with('+') && !line.starts_with("+++") {
                println!("{}", line.green());
            } else {
                println!("{line}");
            }
        }
        println!();
    }
    println!("{}", s.headline().bold());
    for dep in s.pypi_deps().filter(|d| !d.reason.is_empty()) {
        println!("  {} {:<20} PyPI: {}", "·".dimmed(), dep.name, dep.reason);
    }
    for skipped in &s.skipped {
        println!(
            "  {} {:<20} not migrated: {}",
            "!".yellow().bold(),
            skipped.line,
            skipped.reason
        );
    }
    for note in &s.notes {
        println!("  {} {note}", "note:".cyan().bold());
    }
    if let Some(install) = &s.install {
        println!();
        for step in &install.steps {
            let mark = if step.ok {
                "✓".green().bold()
            } else {
                "✗".red().bold()
            };
            println!("  {mark} {:<24} — {}", step.name.bold(), step.detail);
        }
    }
    if s.written || s.install.is_some() {
        println!(
            "\nThe files it was made from were left untouched; CI and pip can keep using them."
        );
    }
    if let Some(next) = &s.next_command {
        println!("\nNext: {}", next.bold());
    }
}

pub async fn install_pixi() -> crate::Result<()> {
    println!(
        "Installing pixi with its official installer ({})…",
        pixi_install::installer_url()
    );
    let installed = pixi_install::install().await?;
    if installed.already_installed {
        println!(
            "pixi {} is already installed at {}.",
            installed.version,
            installed.path.display()
        );
    } else {
        println!(
            "{} pixi {} installed at {}.",
            "✓".green().bold(),
            installed.version,
            installed.path.display()
        );
        println!(
            "Shells opened before the install do not see it yet; refresh PATH with:\n  {}",
            pixi_install::refresh_path_command()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags() {
        let o = options(
            &v(&[
                "--path",
                "x",
                "--write",
                "--platform",
                "linux-64",
                "--platforms=osx-arm64,win-64",
            ]),
            MigrationSource::Auto,
        );
        assert!(o.write && !o.install);
        assert_eq!(o.extra_platforms, ["linux-64", "osx-arm64,win-64"]);
        assert!(!options(&[], MigrationSource::Auto).write);
    }
}

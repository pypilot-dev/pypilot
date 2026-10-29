//! `pypilot --version` and `pypilot version`.
//!
//! `--version` prints exactly `pypilot X.Y.Z` and nothing else. Every wrapper
//! parses it: pypilot-pip's `parse_version`, the VS Code extension's
//! `probeVersion`, and the Zed extension's `parse_helper_version`, which reads
//! the whole of stdout and rejects anything after the version, so released
//! Zed extensions would refuse an engine whose `--version` grew a second line.
//!
//! The details (commit, target, data tables) are in `pypilot version` and
//! `--version --verbose`; `--json` gives them to machines.

use crate::matrix::status::{self, InUse};
use crate::settings::Settings;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: Option<&str> = option_env!("PYPILOT_GIT_COMMIT");
pub const COMMIT_DATE: Option<&str> = option_env!("PYPILOT_GIT_DATE");
pub const TARGET: Option<&str> = option_env!("PYPILOT_TARGET");
pub const DATA_DATE: Option<&str> = option_env!("PYPILOT_DATA_DATE");

/// `mode` is how it was invoked: `version`, `--version` or `-V`.
pub fn run(mode: &str, args: &[String]) {
    let json = args.iter().any(|a| a == "--json");
    let verbose = mode == "version" || args.iter().any(|a| a == "--verbose" || a == "-v");
    if json {
        println!("{}", json_text());
    } else if verbose {
        print!("{}", verbose_text());
    } else {
        println!("pypilot {VERSION}");
    }
}

fn settings() -> Settings {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    Settings::load(&cwd)
}

fn verbose_text() -> String {
    let ttl = settings().data_refresh_days;
    // The first line stays `pypilot X.Y.Z`, whatever follows.
    let mut out = format!("pypilot {VERSION}\n");
    if let Some(commit) = COMMIT {
        match COMMIT_DATE {
            Some(date) => out.push_str(&format!("commit   {commit} ({date})\n")),
            None => out.push_str(&format!("commit   {commit}\n")),
        }
    }
    if let Some(target) = TARGET {
        out.push_str(&format!("target   {target}\n"));
    }
    let bundled = DATA_DATE
        .map(|d| format!("bundled {d}"))
        .unwrap_or_else(|| "bundled".to_string());
    out.push_str(&format!(
        "data     {bundled}; {}\n",
        status::doctor_line(ttl)
    ));
    for t in status::local(ttl) {
        let schema = t
            .bundled_schema
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into());
        let source = match (t.in_use, t.cache_age) {
            (InUse::Cache, Some(age)) => format!("cache, refreshed {}", status::ago(age)),
            _ => "bundled".to_string(),
        };
        out.push_str(&format!("  {:<26} schema {schema:<3} {source}\n", t.name));
    }
    out
}

fn json_text() -> String {
    let ttl = settings().data_refresh_days;
    let value = serde_json::json!({
        "pypilot": VERSION,
        "command": "version",
        "ok": true,
        "version": VERSION,
        "commit": COMMIT,
        "commit_date": COMMIT_DATE,
        "target": TARGET,
        "data": {
            "bundled_date": DATA_DATE,
            "refresh_days": ttl,
            "summary": status::doctor_line(ttl),
            "tables": status::local(ttl),
        },
    });
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_output_keeps_the_parsable_first_line() {
        let text = verbose_text();
        assert_eq!(
            text.lines().next(),
            Some(format!("pypilot {VERSION}").as_str())
        );
        assert!(text.contains("nvidia.json"));
    }

    #[test]
    fn json_has_the_shared_shape() {
        let v: serde_json::Value = serde_json::from_str(&json_text()).unwrap();
        assert_eq!(v["pypilot"], VERSION);
        assert_eq!(v["command"], "version");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["tables"].as_array().map(Vec::len), Some(5));
    }

    #[test]
    fn the_target_triple_is_recorded_at_build_time() {
        assert!(TARGET.is_some_and(|t| t.contains('-')));
    }
}

//! How a CLI command prints its result: text, `--json` or `--markdown`.
//!
//! Every `--json` document has the same top level (see USAGE.md, "JSON
//! output"):
//!
//! ```json
//! { "pypilot": "1.2.2", "command": "doctor", "ok": true, ... }
//! ```
//!
//! followed by the command's own fields. With `--json` nothing else goes to
//! stdout; progress lines go to stderr. A command that fails before it has a
//! report prints `{ ..., "ok": false, "error": "..." }`. Exit codes do not
//! depend on the format.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Markdown,
}

impl OutputFormat {
    /// `--json` or `--markdown` (`--format json|markdown|text` too). JSON
    /// wins when both are given: it is what scripts ask for.
    pub fn from_args(args: &[String]) -> OutputFormat {
        let format = super::flag_value(args, "--format");
        if args.iter().any(|a| a == "--json") || format.as_deref() == Some("json") {
            OutputFormat::Json
        } else if args.iter().any(|a| a == "--markdown" || a == "--md")
            || matches!(format.as_deref(), Some("markdown" | "md"))
        {
            OutputFormat::Markdown
        } else {
            OutputFormat::Text
        }
    }

    pub fn is_json(self) -> bool {
        self == OutputFormat::Json
    }
}

/// Set once a JSON document went to stdout, so a later error does not print
/// a second one.
static EMITTED: AtomicBool = AtomicBool::new(false);

/// The shared top level. A struct (not `json!`) so the keys keep this order.
#[derive(Serialize)]
struct Document<'a, T: Serialize> {
    pypilot: &'static str,
    command: &'a str,
    ok: bool,
    #[serde(flatten)]
    report: &'a T,
}

/// The document for `command`, as pretty-printed text.
pub fn to_json<T: Serialize>(command: &str, ok: bool, report: &T) -> crate::Result<String> {
    Ok(serde_json::to_string_pretty(&Document {
        pypilot: env!("CARGO_PKG_VERSION"),
        command,
        ok,
        report,
    })?)
}

/// Print the document for `command` on stdout. `report` must serialize as
/// an object; its fields follow `ok`.
pub fn print_json<T: Serialize>(command: &str, ok: bool, report: &T) -> crate::Result<()> {
    let text = to_json(command, ok, report)?;
    println!("{text}");
    EMITTED.store(true, Ordering::Relaxed);
    Ok(())
}

#[derive(Serialize)]
struct ErrorReport<'a> {
    error: &'a str,
}

/// After a failed `--json` run: the error as a document, unless the command
/// already printed its report (whose `ok` says it failed).
pub fn print_error(command: &str, error: &anyhow::Error) {
    if EMITTED.load(Ordering::Relaxed) {
        return;
    }
    let message = format!("{error:#}");
    let _ = print_json(command, false, &ErrorReport { error: &message });
}

/// A progress line: stdout for text, stderr when stdout carries JSON.
pub fn progress(format: OutputFormat, line: impl std::fmt::Display) {
    if format.is_json() {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags() {
        assert_eq!(OutputFormat::from_args(&v(&["numpy"])), OutputFormat::Text);
        assert_eq!(
            OutputFormat::from_args(&v(&["numpy", "--json"])),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::from_args(&v(&["--markdown"])),
            OutputFormat::Markdown
        );
        assert_eq!(
            OutputFormat::from_args(&v(&["--format=json"])),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::from_args(&v(&["--markdown", "--json"])),
            OutputFormat::Json
        );
    }

    #[test]
    fn every_document_starts_with_the_same_three_keys() {
        #[derive(Serialize)]
        struct R {
            steps: Vec<u8>,
        }
        let text = to_json("setup", false, &R { steps: vec![1] }).unwrap();
        let expected_prefix = format!(
            "{{\n  \"pypilot\": \"{}\",\n  \"command\": \"setup\",\n  \"ok\": false,\n  \"steps\": [",
            env!("CARGO_PKG_VERSION")
        );
        assert!(text.starts_with(&expected_prefix), "{text}");
        let err = to_json("check", false, &ErrorReport { error: "boom" }).unwrap();
        let v: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(v["error"], "boom");
        assert_eq!(v["ok"], false);
    }
}

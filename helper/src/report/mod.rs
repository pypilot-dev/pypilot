//! Reports shared by the CLI and the LSP.
//!
//! The data behind `pypilot check` ([`check::CheckReport`]) and `pypilot
//! doctor --json` ([`doctor::DoctorReport`]), and the Markdown the editor's
//! "Show details" report and `--markdown` print ([`markdown`]). One builder per
//! report, so the terminal, the JSON and the editor say the same thing.

pub mod check;
pub mod doctor;
pub mod markdown;

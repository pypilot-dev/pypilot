//! `pypilot update-data`: force-refresh the bundled driver/framework/import
//! tables from the project repo, bypassing the TTL.
//!
//! Global, not project-scoped: the datasets are not per-workspace, so unlike
//! every other CLI mode this one takes no `--path`.

use crate::matrix::refresh;

use super::output::{self, OutputFormat};

/// `update-data --json`: one entry per dataset.
#[derive(serde::Serialize)]
struct Refreshed<'a> {
    datasets: Vec<Dataset<'a>>,
}

#[derive(serde::Serialize)]
struct Dataset<'a> {
    name: &'a str,
    ok: bool,
    error: Option<&'a str>,
}

pub async fn run(format: OutputFormat) -> crate::Result<()> {
    output::progress(format, "Refreshing PyPilot's bundled data...");
    let results = refresh::refresh_all().await;

    if format.is_json() {
        let datasets: Vec<Dataset> = results
            .iter()
            .map(|(name, outcome)| Dataset {
                name,
                ok: outcome.is_ok(),
                error: outcome.as_ref().err().map(String::as_str),
            })
            .collect();
        let ok = datasets.iter().all(|d| d.ok);
        // The exit code stays 0 either way: the bundled snapshot still works.
        return output::print_json("update-data", ok, &Refreshed { datasets });
    }

    let mut any_failed = false;
    for (name, outcome) in results {
        match outcome {
            Ok(()) => println!("  ok    {name}"),
            Err(e) => {
                any_failed = true;
                println!("  failed {name}: {e}");
            }
        }
    }

    if any_failed {
        println!("\nOne or more datasets could not be refreshed; PyPilot keeps using the bundled snapshot for those.");
    } else {
        println!("\nAll datasets refreshed.");
    }
    Ok(())
}

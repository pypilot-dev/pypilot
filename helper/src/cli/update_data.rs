//! `pypilot update-data`: force-refresh the bundled driver/framework/import
//! tables from the project repo, bypassing the TTL.
//!
//! Global, not project-scoped: the datasets are not per-workspace, so unlike
//! every other CLI mode this one takes no `--path`.

use crate::matrix::refresh;

pub async fn run() -> crate::Result<()> {
    println!("Refreshing PyPilot's bundled data...");
    let results = refresh::refresh_all().await;

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

/// `pypilot update-data --check`: compare the tables in use with `main`
/// without writing anything. Exit 0 = current, 1 = `main` differs, 2 = a
/// table could not be fetched.
pub async fn check(settings: &crate::settings::Settings, json: bool) -> crate::Result<()> {
    use crate::matrix::status::{self, InUse};

    let ttl = settings.data_refresh_days;
    let tables = status::check_remote(ttl).await;
    let errors = tables.iter().filter(|t| t.main == "error").count();
    let stale = tables.iter().filter(|t| t.main == "differs").count();
    let (code, verdict) = match (errors, stale) {
        (0, 0) => (0, "current"),
        (0, _) => (1, "stale"),
        _ => (2, "error"),
    };

    if json {
        let out = serde_json::json!({
            "pypilot": env!("CARGO_PKG_VERSION"),
            "command": "update-data",
            "ok": code == 0,
            "status": verdict,
            "refresh_days": ttl,
            "summary": status::doctor_line(ttl),
            "cache_dir": crate::core::platform::cache_dir().join("data"),
            "tables": tables,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("PyPilot data tables: {}", status::doctor_line(ttl));
        if ttl == 0 {
            println!("  data_refresh_days = 0: the engine never reads the cache.");
        }
        for t in &tables {
            let local = match (t.local.in_use, t.local.cache_age) {
                (InUse::Cache, Some(age)) => format!("cache, refreshed {}", status::ago(age)),
                (InUse::Bundled, Some(age)) => {
                    format!("bundled (cache from {} not used)", status::ago(age))
                }
                _ => "bundled".to_string(),
            };
            let main = match t.main {
                "same" => "same as main".to_string(),
                "differs" => format!(
                    "main differs (schema {})",
                    t.main_schema
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".into())
                ),
                "missing" => "not on main (only the bundled copy exists)".to_string(),
                _ => format!("main: {}", t.error.as_deref().unwrap_or("fetch failed")),
            };
            println!(
                "  {:<26} schema {:<3} in use: {:<34} {main}",
                t.local.name,
                t.local
                    .bundled_schema
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".into()),
                local
            );
        }
        match code {
            0 => println!("\nThe tables in use match main."),
            1 => println!(
                "\n{stale} table(s) differ from main; `pypilot update-data` fetches them now."
            ),
            _ => println!("\n{errors} table(s) could not be fetched; check the network."),
        }
    }
    if code == 0 {
        Ok(())
    } else {
        Err(super::Exit(code).into())
    }
}

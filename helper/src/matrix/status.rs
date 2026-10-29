//! The data tables as a user's engine sees them: which copy is in use
//! (bundled or a refreshed cache), how old the cache is, when the next
//! refresh is due, and (on request) whether `main` has something different.
//!
//! [`local`] and [`doctor_line`] only stat and read the cache, like
//! [`refresh::read`]; [`check_remote`] is the one network call, and it writes
//! nothing.

use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::matrix::refresh::{self, DATASETS};

const DAY: u64 = 24 * 60 * 60;

/// Which copy of a table the engine reads right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InUse {
    Bundled,
    Cache,
}

/// A table's local state.
#[derive(Debug, Clone, Serialize)]
pub struct Local {
    pub name: &'static str,
    pub bundled_schema: Option<i64>,
    /// Age of the refreshed copy in the cache, if there is one.
    #[serde(skip)]
    pub cache_age: Option<Duration>,
    pub cache_age_days: Option<f64>,
    pub cache_schema: Option<i64>,
    pub in_use: InUse,
}

fn schema_of(text: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("schema_version")?
        .as_i64()
}

fn age_of(path: &std::path::Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO),
    )
}

/// Every table's local state under a `ttl_days` refresh setting.
pub fn local(ttl_days: u32) -> Vec<Local> {
    DATASETS
        .iter()
        .map(|(name, bundled)| {
            let path = refresh::cache_path(name);
            let cache_age = age_of(&path);
            let cache_text = cache_age.and_then(|_| std::fs::read_to_string(&path).ok());
            let cache_schema = cache_text.as_deref().and_then(schema_of);
            let cache_parses = cache_text
                .as_deref()
                .is_some_and(|t| serde_json::from_str::<serde_json::Value>(t).is_ok());
            // The same rule `refresh::read` applies.
            let fresh = ttl_days > 0
                && cache_parses
                && cache_age.is_some_and(|a| a.as_secs() <= u64::from(ttl_days) * DAY);
            Local {
                name,
                bundled_schema: schema_of(bundled),
                cache_age,
                cache_age_days: cache_age
                    .map(|a| (a.as_secs_f64() / DAY as f64 * 10.0).round() / 10.0),
                cache_schema,
                in_use: if fresh { InUse::Cache } else { InUse::Bundled },
            }
        })
        .collect()
}

/// "3 days ago", "5 hours ago", "just now".
pub fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 * 60 {
        "just now".to_string()
    } else if s < DAY {
        plural(s / 3600, "hour") + " ago"
    } else {
        plural(s / DAY, "day") + " ago"
    }
}

/// "4 days", "less than a day".
fn span(d: Duration) -> String {
    let s = d.as_secs();
    if s < DAY {
        "less than a day".to_string()
    } else {
        plural(s / DAY, "day")
    }
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// The one-line summary `doctor` prints, e.g. `refreshed 3 days ago (next
/// in 4 days)`. No network.
pub fn doctor_line(ttl_days: u32) -> String {
    summarize(&local(ttl_days), ttl_days)
}

fn summarize(tables: &[Local], ttl_days: u32) -> String {
    if ttl_days == 0 {
        return "bundled (refresh disabled)".to_string();
    }
    let ttl = Duration::from_secs(u64::from(ttl_days) * DAY);
    let cached: Vec<Duration> = tables.iter().filter_map(|t| t.cache_age).collect();
    let Some(newest) = cached.iter().min().copied() else {
        return "bundled (not refreshed yet; the next scan fetches them)".to_string();
    };
    let in_use = tables.iter().filter(|t| t.in_use == InUse::Cache).count();
    if in_use == 0 {
        return format!(
            "bundled (last refreshed {}, overdue; the next scan refreshes)",
            ago(newest)
        );
    }
    // The table refreshed longest ago is the next one due.
    let oldest_in_use = tables
        .iter()
        .filter(|t| t.in_use == InUse::Cache)
        .filter_map(|t| t.cache_age)
        .max()
        .unwrap_or(newest);
    let mut line = format!(
        "refreshed {} (next in {})",
        ago(newest),
        span(ttl.saturating_sub(oldest_in_use))
    );
    if in_use < tables.len() {
        line.push_str(&format!(
            "; {} of {} bundled",
            tables.len() - in_use,
            tables.len()
        ));
    }
    line
}

/// How `main` compares with a table's local copies.
#[derive(Debug, Clone, Serialize)]
pub struct Remote {
    #[serde(flatten)]
    pub local: Local,
    /// `same`, `differs`, `missing` (404 on main) or `error`.
    pub main: &'static str,
    pub main_schema: Option<i64>,
    /// Whether `main` matches the copy built into this engine.
    pub main_matches_bundled: Option<bool>,
    pub error: Option<String>,
}

/// Fetch every table from `main` and compare it with the copy in use.
/// Compares parsed JSON, so line endings and formatting do not count.
/// Writes nothing.
pub async fn check_remote(ttl_days: u32) -> Vec<Remote> {
    let client = refresh::client();
    let tables = local(ttl_days);
    let fetches = DATASETS
        .iter()
        .map(|(name, _)| refresh::fetch(&client, name));
    let fetched = futures::future::join_all(fetches).await;
    tables
        .into_iter()
        .zip(fetched)
        .zip(DATASETS)
        .map(|((local, fetched), (name, bundled))| match fetched {
            Ok(text) => {
                let remote: Option<serde_json::Value> = serde_json::from_str(&text).ok();
                let in_use_text = match local.in_use {
                    InUse::Bundled => bundled.to_string(),
                    InUse::Cache => refresh::read(name, bundled, ttl_days),
                };
                let parse = |t: &str| serde_json::from_str::<serde_json::Value>(t).ok();
                let same = remote.is_some() && remote == parse(&in_use_text);
                Remote {
                    main: if same { "same" } else { "differs" },
                    main_schema: schema_of(&text),
                    main_matches_bundled: Some(remote.is_some() && remote == parse(bundled)),
                    error: None,
                    local,
                }
            }
            Err(e) => Remote {
                main: if e == refresh::NOT_ON_MAIN {
                    "missing"
                } else {
                    "error"
                },
                main_schema: None,
                main_matches_bundled: None,
                error: Some(e),
                local,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(age_days: Option<u64>, in_use: InUse) -> Local {
        Local {
            name: "nvidia.json",
            bundled_schema: Some(3),
            cache_age: age_days.map(|d| Duration::from_secs(d * DAY + 60)),
            cache_age_days: None,
            cache_schema: Some(3),
            in_use,
        }
    }

    #[test]
    fn doctor_line_cases() {
        assert_eq!(summarize(&[], 0), "bundled (refresh disabled)");
        assert!(summarize(&[table(None, InUse::Bundled)], 7).starts_with("bundled (not refreshed"));
        assert_eq!(
            summarize(
                &[table(Some(3), InUse::Cache), table(Some(2), InUse::Cache)],
                7
            ),
            "refreshed 2 days ago (next in 3 days)"
        );
        assert_eq!(
            summarize(
                &[table(Some(3), InUse::Cache), table(None, InUse::Bundled)],
                7
            ),
            "refreshed 3 days ago (next in 3 days); 1 of 2 bundled"
        );
        assert!(summarize(&[table(Some(9), InUse::Bundled)], 7).contains("overdue"));
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(Duration::from_secs(5)), "just now");
        assert_eq!(ago(Duration::from_secs(2 * 3600)), "2 hours ago");
        assert_eq!(ago(Duration::from_secs(DAY + 5)), "1 day ago");
        assert_eq!(span(Duration::from_secs(3600)), "less than a day");
    }

    #[test]
    fn bundled_tables_have_a_schema_version() {
        for t in local(0) {
            assert!(t.bundled_schema.is_some(), "{}", t.name);
            assert_eq!(t.in_use, InUse::Bundled);
        }
    }
}

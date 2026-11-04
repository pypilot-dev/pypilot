//! `data/known_breakages.json`, parsed.
//!
//! Each section is read row by row: a row this engine cannot parse (a field
//! retyped by mistake, a kind added after this release) is skipped rather than
//! taking the whole table down with it. Unknown fields are ignored, so newer
//! rows with extra fields still work here.

use std::sync::OnceLock;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::matrix::{refresh, KNOWN_BREAKAGES_JSON};

/// A compiled package, or one with an uncapped dependency, that breaks once
/// that dependency reaches `breaks_from`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ApiRemoval {
    pub id: String,
    /// `abi` (built against the old binary interface) or `api` (calls an API
    /// the dependency removed).
    pub kind: String,
    pub package: String,
    /// First release of `package` that works with the new dependency.
    pub fixed_in: String,
    pub dependency: String,
    /// First release of `dependency` that breaks older `package` releases.
    pub breaks_from: String,
    /// Only on this Python or newer (numpy 2 needs 3.9, for example).
    #[serde(default)]
    pub python_min: Option<String>,
    /// Only below this Python (the stdlib covers it from there on).
    #[serde(default)]
    pub python_below: Option<String>,
    /// The requirement that keeps the dependency below the break: `numpy<2`.
    pub constraint: String,
    /// `upgrade` when upgrading `package` is the better fix than capping
    /// the dependency (a cap that holds back security fixes).
    #[serde(default)]
    pub prefer: Option<String>,
    pub symptom: String,
    pub source: String,
}

/// A package that stops working on a newer Python.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PythonRemoval {
    pub id: String,
    pub package: String,
    /// First release that works there; absent when none does.
    #[serde(default)]
    pub fixed_in: Option<String>,
    /// The first Python it breaks on.
    pub python_min: String,
    /// The first release that breaks there; older releases work (SQLAlchemy
    /// 1.4 runs on Python 3.13, 2.0.0 to 2.0.30 do not). Absent: every
    /// release below `fixed_in`.
    #[serde(default)]
    pub breaks_from: Option<String>,
    /// What to use instead when no release is fixed.
    #[serde(default)]
    pub replacement: Option<String>,
    /// `replacement` provides the same import, so swapping is safe.
    #[serde(default)]
    pub drop_in: bool,
    pub symptom: String,
    pub source: String,
}

/// A distribution that writes over another one's module, shadows the
/// standard library, or is a dead name for a real package.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Shadowing {
    pub id: String,
    /// `clash`, `backport` or `deprecated`.
    pub kind: String,
    pub dist: String,
    pub module: String,
    /// `clash`: the other distribution that provides `module`.
    #[serde(default)]
    pub other: Option<String>,
    /// The distribution to keep (`clash`) or install instead (`deprecated`).
    #[serde(default)]
    pub keep: Option<String>,
    /// `backport`: the Python whose standard library has `module`.
    #[serde(default)]
    pub stdlib_since: Option<String>,
    /// `info` for harmless leftovers; warning otherwise.
    #[serde(default)]
    pub severity: Option<String>,
    pub symptom: String,
    pub source: String,
}

/// A module the project imports that a dependency stopped shipping.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModuleRemoval {
    pub id: String,
    pub module: String,
    pub dependency: String,
    pub breaks_from: String,
    pub constraint: String,
    #[serde(default)]
    pub migrate: Option<String>,
    pub symptom: String,
    pub source: String,
}

/// A standard library module a Python release removed.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct StdlibRemoval {
    pub module: String,
    pub removed_in: String,
    /// A PyPI distribution that provides the module again.
    #[serde(default)]
    pub replacement: Option<String>,
    /// What to move the code to instead.
    #[serde(default)]
    pub migrate: Option<String>,
    pub source: String,
}

/// A module attribute a distribution release or a Python removed, which
/// source still uses (`np.float`, `from pydantic import BaseSettings`,
/// `collections.Mapping`).
///
/// A distribution row has `dependency`, `breaks_from` and `constraint`; a
/// standard library row has `removed_in`.
///
/// A row with `calls` is about a method on an object the code builds
/// (`db.engine.execute(...)`, SQLAlchemy 2), not a module attribute: its
/// `symbol` only names it (`Engine.execute`) and the uses are found by the
/// call patterns ([`crate::core::imports::CallPattern`]), in files that
/// import one of `calls_in` (default: `module`). A `symbol` with a dot never
/// matches a module attribute, so an engine without `calls` finds nothing.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SymbolRemoval {
    pub id: String,
    /// The module the symbol was looked up in, dotted.
    pub module: String,
    pub symbol: String,
    /// The first Python without it (standard library rows).
    #[serde(default)]
    pub removed_in: Option<String>,
    /// The distribution that removed it (distribution rows).
    #[serde(default)]
    pub dependency: Option<String>,
    /// First release of `dependency` without it.
    #[serde(default)]
    pub breaks_from: Option<String>,
    /// The requirement that keeps the old API: `numpy<1.24`.
    #[serde(default)]
    pub constraint: Option<String>,
    /// A distribution the replacement code needs: `pydantic-settings`.
    #[serde(default)]
    pub add: Option<String>,
    /// The error, when it is not the plain AttributeError or ImportError.
    #[serde(default)]
    pub symptom: Option<String>,
    /// What to write instead.
    pub replacement: String,
    pub source: String,
    /// Call patterns that use the removed method: `engine.execute(`,
    /// `conn.execute("`, `*.append(ignore_index=`.
    #[serde(default)]
    pub calls: Vec<String>,
    /// Top-level modules a file must import for `calls` to be looked for
    /// in it; `module`'s top-level name when empty.
    #[serde(default)]
    pub calls_in: Vec<String>,
}

impl SymbolRemoval {
    /// The modules a file must import for this row's calls to count.
    pub fn call_gate(&self) -> Vec<String> {
        if self.calls_in.is_empty() {
            vec![self
                .module
                .split('.')
                .next()
                .unwrap_or(&self.module)
                .to_string()]
        } else {
            self.calls_in.clone()
        }
    }
}

/// A row's call patterns, parsed, for the source scan.
#[derive(Debug, Clone)]
pub struct CallTarget {
    pub module: String,
    pub symbol: String,
    pub patterns: Vec<crate::core::imports::CallPattern>,
    /// See [`SymbolRemoval::call_gate`].
    pub gate: Vec<String>,
}

/// The whole table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub api_removals: Vec<ApiRemoval>,
    pub python_removals: Vec<PythonRemoval>,
    pub shadowing: Vec<Shadowing>,
    pub module_removals: Vec<ModuleRemoval>,
    pub stdlib_removals: Vec<StdlibRemoval>,
    pub symbol_removals: Vec<SymbolRemoval>,
}

impl Table {
    /// Parse the table, skipping rows that do not parse. `None` when the
    /// text is not a JSON object at all.
    pub fn load(json: &str) -> Option<Table> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        value.as_object()?;
        Some(Table {
            api_removals: rows(&value, "api_removals"),
            python_removals: rows(&value, "python_removals"),
            shadowing: rows(&value, "shadowing"),
            module_removals: rows(&value, "module_removals"),
            stdlib_removals: rows(&value, "stdlib_removals"),
            symbol_removals: rows(&value, "symbol_removals"),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.api_removals.is_empty()
            && self.python_removals.is_empty()
            && self.shadowing.is_empty()
            && self.module_removals.is_empty()
            && self.stdlib_removals.is_empty()
            && self.symbol_removals.is_empty()
    }

    /// Every `(module, symbol)` pair `symbol_removals` names as a module
    /// attribute, for the scan.
    pub fn symbol_targets(&self) -> Vec<(String, String)> {
        self.symbol_removals
            .iter()
            .filter(|r| r.calls.is_empty())
            .map(|r| (r.module.clone(), r.symbol.clone()))
            .collect()
    }

    /// The method-call rows, with the patterns this engine can parse.
    pub fn call_targets(&self) -> Vec<CallTarget> {
        self.symbol_removals
            .iter()
            .filter_map(|r| {
                let patterns: Vec<_> = r
                    .calls
                    .iter()
                    .filter_map(|c| crate::core::imports::CallPattern::parse(c))
                    .collect();
                (!patterns.is_empty()).then(|| CallTarget {
                    module: r.module.clone(),
                    symbol: r.symbol.clone(),
                    patterns,
                    gate: r.call_gate(),
                })
            })
            .collect()
    }

    /// The `stdlib_removals` row for `module`, if Python removed it.
    pub fn stdlib_removal(&self, module: &str) -> Option<&StdlibRemoval> {
        self.stdlib_removals.iter().find(|r| r.module == module)
    }

    /// The `module_removals` row for `module`, if a dependency dropped it.
    pub fn module_removal(&self, module: &str) -> Option<&ModuleRemoval> {
        self.module_removals.iter().find(|r| r.module == module)
    }
}

fn rows<T: DeserializeOwned>(value: &serde_json::Value, key: &str) -> Vec<T> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| serde_json::from_value(r.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The table for this process: the refreshed copy when it is fresh and
/// parses, else the bundled one. Parsed once, like the other tables.
pub fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let json = refresh::read("known_breakages.json", KNOWN_BREAKAGES_JSON, 7);
        Table::load(&json)
            .filter(|t| !t.is_empty())
            .or_else(|| Table::load(KNOWN_BREAKAGES_JSON))
            .unwrap_or_default()
    })
}

/// The bundled table, for tests that must not depend on a refreshed cache.
pub fn bundled() -> Table {
    Table::load(KNOWN_BREAKAGES_JSON).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_table_parses_every_row() {
        let raw: serde_json::Value = serde_json::from_str(KNOWN_BREAKAGES_JSON).unwrap();
        let t = bundled();
        let count = |k: &str| raw[k].as_array().unwrap().len();
        assert_eq!(t.api_removals.len(), count("api_removals"));
        assert_eq!(t.python_removals.len(), count("python_removals"));
        assert_eq!(t.shadowing.len(), count("shadowing"));
        assert_eq!(t.module_removals.len(), count("module_removals"));
        assert_eq!(t.stdlib_removals.len(), count("stdlib_removals"));
        assert_eq!(t.symbol_removals.len(), count("symbol_removals"));
        // Each row is a Python removal or a distribution removal.
        for r in &t.symbol_removals {
            let by_dist =
                r.dependency.is_some() && r.breaks_from.is_some() && r.constraint.is_some();
            assert!(by_dist != r.removed_in.is_some(), "{}", r.id);
        }
        assert!(t.stdlib_removal("distutils").is_some());
        assert!(t.module_removal("pkg_resources").is_some());
    }

    #[test]
    fn a_bad_row_is_skipped_not_fatal() {
        let t = Table::load(
            r#"{"stdlib_removals": [
                {"module": "imp", "removed_in": "3.12", "source": "u"},
                {"module": 7},
                {"module": "cgi", "removed_in": "3.13", "source": "u", "new_field": [1]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(t.stdlib_removals.len(), 2);
        assert!(t.api_removals.is_empty());
        assert!(Table::load("[]").is_none());
        assert!(Table::load("not json").is_none());
    }
}

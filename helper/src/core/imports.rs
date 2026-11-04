//! Import extraction from Python source.
//!
//! Deliberately a line scanner rather than a parser. F4 only needs the top-level
//! module of each import plus its position in the buffer, and a scanner stays
//! fast enough to run on every keystroke debounce without a syntax tree.
//!
//! It does track string state, because an `import` inside a docstring is not an
//! import and squiggling it would be wrong. Comments are stripped the same way.

/// One imported module, with the buffer range covering the module path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRef {
    /// Top-level module: `cv2` from `import cv2.aruco`, `os` from `from os.path import join`.
    pub module: String,
    /// The local name bound to the package *root*, when there is one.
    ///
    /// `import mediapipe` binds `mediapipe`, `import mediapipe as mp` binds
    /// `mp`, and `import a.b` binds `a`. It is `None` for `from x import y` and
    /// for `import a.b as c`, where the name refers to something below the
    /// root; attribute checks against the root would be wrong for those.
    pub binding: Option<String>,
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based character offsets covering the module path as written.
    pub start: u32,
    pub end: u32,
}

/// A `name.attribute` access found in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrUse {
    /// The local name on the left, e.g. `mp`.
    pub base: String,
    /// The attribute on the right, e.g. `solutions`.
    pub attr: String,
    pub line: u32,
    /// Range covering `base.attr`.
    pub start: u32,
    pub end: u32,
}

/// Extract every absolute import in `source`.
///
/// Relative imports (`from . import x`) are skipped: they name modules inside
/// the project, never a PyPI distribution.
pub fn extract(source: &str) -> Vec<ImportRef> {
    let mut out = Vec::new();
    let mut open_delim: Option<&'static str> = None;

    for (lineno, raw) in source.lines().enumerate() {
        let (code, next_delim) = strip_strings_and_comments(raw, open_delim);
        open_delim = next_delim;

        let trimmed = code.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        // Column offset introduced by leading indentation.
        let indent = code.len() - trimmed.len();

        if let Some(rest) = trimmed.strip_prefix("import ") {
            let base = indent + "import ".len();
            collect_import_list(rest, lineno as u32, base, &mut out);
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            let base = indent + "from ".len();
            collect_from(rest, lineno as u32, base, &mut out);
        }
    }
    out
}

/// Find `base.attr` accesses for the given local names.
///
/// Only the first attribute after the name is taken: `mp.solutions.hands`
/// yields `solutions`, which is the one that has to exist on the package for
/// the rest of the chain to be reachable. Import statements are skipped, since
/// `import a.b` is not an attribute access.
pub fn extract_attr_uses(source: &str, bases: &[String]) -> Vec<AttrUse> {
    if bases.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut open_delim: Option<&'static str> = None;

    for (lineno, raw) in source.lines().enumerate() {
        let (code, next_delim) = strip_strings_and_comments(raw, open_delim);
        open_delim = next_delim;

        let trimmed = code.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("import ") || trimmed.starts_with("from ") {
            continue;
        }

        let chars: Vec<char> = code.chars().collect();
        for base in bases {
            let base_chars: Vec<char> = base.chars().collect();
            let mut i = 0;
            while i + base_chars.len() < chars.len() {
                if chars[i..i + base_chars.len()] != base_chars[..] {
                    i += 1;
                    continue;
                }
                // Must be a standalone name followed by a dot. A preceding dot
                // means it is somebody else's attribute (`self.mp.foo`), not the
                // module bound by the import.
                let before_ok = i == 0 || (!is_ident_char(chars[i - 1]) && chars[i - 1] != '.');
                let dot_at = i + base_chars.len();
                if !before_ok || chars[dot_at] != '.' {
                    i += 1;
                    continue;
                }

                let attr: String = chars[dot_at + 1..]
                    .iter()
                    .take_while(|c| is_ident_char(**c))
                    .collect();
                if attr.is_empty() || attr.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    i = dot_at + 1;
                    continue;
                }

                let end = dot_at + 1 + attr.chars().count();
                out.push(AttrUse {
                    base: base.clone(),
                    attr,
                    line: lineno as u32,
                    start: i as u32,
                    end: end as u32,
                });
                i = end;
            }
        }
    }
    out
}

/// Cell magics whose body is still Python run by the kernel, so its imports are
/// real. Any other cell magic (`%%bash`, `%%writefile`, `%%html`) hands the body
/// to something else, and scanning it as Python would invent problems.
const PYTHON_CELL_MAGICS: &[&str] = &[
    "time", "timeit", "capture", "prun", "debug", "python", "python3",
];

/// The Python code cells of an `.ipynb`, IPython syntax masked (see
/// [`mask_ipython`]), one after another. `None` for what is not a notebook,
/// or one whose kernel is not Python.
pub fn notebook_code(json: &str) -> Option<String> {
    let nb: serde_json::Value = serde_json::from_str(json).ok()?;
    let language = nb
        .pointer("/metadata/kernelspec/language")
        .or_else(|| nb.pointer("/metadata/language_info/name"))
        .and_then(|l| l.as_str());
    if language.is_some_and(|l| !l.eq_ignore_ascii_case("python")) {
        return None;
    }
    let mut text = String::new();
    for cell in nb.get("cells")?.as_array()? {
        if cell.get("cell_type").and_then(|t| t.as_str()) != Some("code") {
            continue;
        }
        let source = match cell.get("source") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(parts)) => {
                parts.iter().filter_map(|p| p.as_str()).collect::<String>()
            }
            _ => continue,
        };
        text.push_str(&mask_ipython(&source));
        text.push('\n');
    }
    Some(text)
}

/// Blank out IPython-only syntax in a notebook cell so the scanners see Python.
///
/// Line magics (`%pip install x`), shell escapes (`!ls`, `files = !ls`) and help
/// requests (`?obj`, `obj?`) are not Python. Left in, a stray quote in a shell
/// command opens a "string" that swallows the imports below it, and
/// `!python -c mp.solutions` reads as an attribute access. Lines are blanked
/// rather than removed so every other line keeps its number.
///
/// Only for notebook cells: in a `.py` file a line starting with `%` continues
/// an expression, it is not a magic.
pub fn mask_ipython(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();

    // `%%name` must open the cell. If the kernel does not run the body as
    // Python, none of the cell is ours to check.
    if let Some(first) = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty()) {
        if let Some(magic) = first.strip_prefix("%%") {
            let name = magic.split_whitespace().next().unwrap_or("");
            if !PYTHON_CELL_MAGICS.contains(&name) {
                return "\n".repeat(lines.len());
            }
        }
    }

    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut open_delim: Option<&'static str> = None;
    for raw in lines {
        let in_string = open_delim.is_some();
        let (code, next_delim) = strip_strings_and_comments(raw, open_delim);
        // A masked line's quotes mean nothing, so it must not move string state.
        if !in_string && is_ipython_line(raw, &code) {
            out.push("");
            continue;
        }
        open_delim = next_delim;
        out.push(raw);
    }
    out.join("\n")
}

/// `raw` is the line as written, `code` the same line with strings and comments
/// blanked, so a `?` or `=` inside a string literal never counts.
fn is_ipython_line(raw: &str, code: &str) -> bool {
    let trimmed = raw.trim_start();
    if trimmed.starts_with(['%', '!', '?']) {
        return true;
    }
    // `obj?` / `obj??`. Python has no `?` operator, so outside a string this is
    // always a help request.
    if code.trim_end().ends_with('?') {
        return true;
    }
    // `files = !ls`, `t = %timeit -o f()`. An assignment whose right side starts
    // with `!` or `%` is never valid Python, so this cannot catch real code.
    let chars: Vec<char> = code.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '=' {
            continue;
        }
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        if matches!(prev, Some('=' | '!' | '<' | '>')) || next == Some('=') {
            continue;
        }
        let rhs = chars[i + 1..].iter().find(|c| !c.is_whitespace());
        if matches!(rhs, Some('!' | '%')) {
            return true;
        }
    }
    false
}

/// A use of `module.symbol` found in source: `np.float` after
/// `import numpy as np`, or `from pydantic import BaseSettings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolUse {
    /// The module the symbol is looked up in, dotted: `numpy`, `collections`.
    pub module: String,
    pub symbol: String,
    /// One-based line number.
    pub line: u32,
    /// The code as written: `np.float`, `from pydantic import BaseSettings`.
    pub written: String,
    /// `from module import symbol` rather than an attribute access.
    pub from_import: bool,
}

/// Find uses of the `(module, symbol)` pairs in `targets`: attribute access
/// through whatever name the module is bound to (`import numpy as np` then
/// `np.float`, `import collections` then `collections.Mapping`,
/// `from scipy import integrate` then `integrate.simps`), and `from module
/// import symbol`, parenthesized lists included. One use per pair per line.
///
/// A line scanner like [`extract`]: strings and comments are masked, and a
/// name rebound later in the file is not tracked, which is fine for the
/// module aliases this looks for.
pub fn symbol_uses(source: &str, targets: &[(String, String)]) -> Vec<SymbolUse> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut open_delim: Option<&'static str> = None;
    for raw in source.lines() {
        let (code, next) = strip_strings_and_comments(raw, open_delim);
        open_delim = next;
        lines.push(code);
    }
    let is_target =
        |module: &str, symbol: &str| targets.iter().any(|(m, s)| m == module && s == symbol);

    // Local name -> dotted module it is bound to; and the import lines.
    let mut aliases: std::collections::HashMap<String, String> = Default::default();
    let mut import_lines = std::collections::HashSet::new();
    let mut out: Vec<SymbolUse> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if let Some(rest) = trimmed.strip_prefix("import ") {
            import_lines.insert(i);
            for part in rest.split(',') {
                let mut words = part.split_whitespace();
                let path = words.next().unwrap_or("");
                if !is_module_path(path) {
                    continue;
                }
                match (words.next(), words.next()) {
                    (Some("as"), Some(name)) => {
                        aliases.insert(name.to_string(), path.to_string());
                    }
                    _ => {
                        let root = path.split('.').next().unwrap_or(path);
                        aliases.insert(root.to_string(), root.to_string());
                    }
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("from ") {
            let start = i;
            let mut body = rest.to_string();
            // `from x import (a,\n b)`: read on to the closing parenthesis.
            if body.contains('(') {
                while !body.contains(')') && i + 1 < lines.len() {
                    i += 1;
                    import_lines.insert(i);
                    body.push(' ');
                    body.push_str(lines[i].trim());
                }
            }
            import_lines.insert(start);
            if let Some((module, names)) = body.split_once(" import ") {
                let module = module.trim();
                if is_module_path(module) {
                    let names = names.replace(['(', ')', '\\'], " ");
                    for part in names.split(',') {
                        let mut words = part.split_whitespace();
                        let Some(name) = words.next().filter(|n| is_module_path(n)) else {
                            continue;
                        };
                        let local = match (words.next(), words.next()) {
                            (Some("as"), Some(alias)) => alias,
                            _ => name,
                        };
                        if is_target(module, name) {
                            out.push(SymbolUse {
                                module: module.to_string(),
                                symbol: name.to_string(),
                                line: start as u32 + 1,
                                written: format!("from {module} import {name}"),
                                from_import: true,
                            });
                        }
                        // It may be a submodule (`from scipy import integrate`).
                        aliases.insert(local.to_string(), format!("{module}.{name}"));
                    }
                }
            }
        }
        i += 1;
    }

    for (n, code) in lines.iter().enumerate() {
        if import_lines.contains(&n) {
            continue;
        }
        let chars: Vec<char> = code.chars().collect();
        let mut j = 0;
        while j < chars.len() {
            let starts = is_ident_char(chars[j])
                && !chars[j].is_ascii_digit()
                && (j == 0 || (!is_ident_char(chars[j - 1]) && chars[j - 1] != '.'));
            if !starts {
                j += 1;
                continue;
            }
            // Read `name(.name)*`.
            let mut parts: Vec<String> = Vec::new();
            let mut k = j;
            loop {
                let word: String = chars[k..]
                    .iter()
                    .take_while(|c| is_ident_char(**c))
                    .collect();
                if word.is_empty() {
                    break;
                }
                k += word.chars().count();
                parts.push(word);
                if k < chars.len() && chars[k] == '.' {
                    k += 1;
                } else {
                    break;
                }
            }
            if parts.len() >= 2 {
                if let Some(bound) = aliases.get(&parts[0]) {
                    let bound_len = bound.split('.').count();
                    let mut full: Vec<&str> = bound.split('.').collect();
                    full.extend(parts[1..].iter().map(String::as_str));
                    for cut in bound_len..full.len() {
                        let module = full[..cut].join(".");
                        let symbol = full[cut];
                        let line = n as u32 + 1;
                        let seen = out
                            .iter()
                            .any(|u| u.line == line && u.module == module && u.symbol == symbol);
                        if is_target(&module, symbol) && !seen {
                            // `full[cut]` is `parts[cut - bound_len + 1]`.
                            let written = parts[..cut - bound_len + 2].join(".");
                            out.push(SymbolUse {
                                module,
                                symbol: symbol.to_string(),
                                line,
                                written,
                                from_import: false,
                            });
                        }
                    }
                }
            }
            j = k.max(j + 1);
        }
    }
    out.sort_by_key(|u| u.line);
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `import a.b as x, c` -> refs for `a` and `c`.
fn collect_import_list(rest: &str, line: u32, base: usize, out: &mut Vec<ImportRef>) {
    let mut offset = base;
    for part in rest.split(',') {
        let leading = part.len() - part.trim_start().len();
        let token = part.trim();
        // Drop an `as alias` suffix; the module path is the first word.
        let mut words = token.split_whitespace();
        let path = words.next().unwrap_or("");
        let alias = match (words.next(), words.next()) {
            (Some("as"), Some(name)) => Some(name),
            _ => None,
        };

        if is_module_path(path) {
            // `import a.b` binds `a`; `import a.b as c` binds `c`, which points
            // below the root, so the root has no local name here.
            let dotted = path.contains('.');
            let binding = match (alias, dotted) {
                (Some(name), false) => Some(name.to_string()),
                (Some(_), true) => None,
                (None, _) => path.split('.').next().map(|s| s.to_string()),
            };
            let start = offset + leading;
            push_ref(path, binding, line, start, out);
        }
        // +1 for the comma we split on.
        offset += part.len() + 1;
    }
}

/// `from a.b import c` -> ref for `a`. Relative imports are skipped.
fn collect_from(rest: &str, line: u32, base: usize, out: &mut Vec<ImportRef>) {
    let leading = rest.len() - rest.trim_start().len();
    let path = rest.split_whitespace().next().unwrap_or("");
    // `.mod`, `..pkg` and bare `.` are all project-relative.
    if path.starts_with('.') || !is_module_path(path) {
        return;
    }
    // `from x import y` binds y, which is not the package root.
    push_ref(path, None, line, base + leading, out);
}

fn push_ref(
    path: &str,
    binding: Option<String>,
    line: u32,
    start: usize,
    out: &mut Vec<ImportRef>,
) {
    let Some(top) = path.split('.').next() else {
        return;
    };
    if top.is_empty() {
        return;
    }
    out.push(ImportRef {
        module: top.to_string(),
        binding,
        line,
        start: start as u32,
        end: (start + path.chars().count()) as u32,
    });
}

/// A dotted identifier path, and nothing else.
fn is_module_path(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
}

/// Blank out string literals and comments so their contents are never scanned.
///
/// Returns the masked line plus the triple-quote delimiter still open at the end
/// of it, if any. Masking preserves length so column positions stay correct.
fn strip_strings_and_comments(
    line: &str,
    mut open_delim: Option<&'static str>,
) -> (String, Option<&'static str>) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;

    while i < chars.len() {
        // Inside a triple-quoted string: consume until the closing delimiter.
        if let Some(delim) = open_delim {
            if starts_with_at(&chars, i, delim) {
                open_delim = None;
                out.push_str("   ");
                i += 3;
            } else {
                out.push(' ');
                i += 1;
            }
            continue;
        }

        // Opening triple quote.
        if starts_with_at(&chars, i, "\"\"\"") || starts_with_at(&chars, i, "'''") {
            let delim = if chars[i] == '"' { "\"\"\"" } else { "'''" };
            open_delim = Some(delim);
            out.push_str("   ");
            i += 3;
            continue;
        }

        // Single-quoted string: skip to its close on this line.
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            out.push(' ');
            i += 1;
            while i < chars.len() {
                let escaped = chars[i] == '\\';
                out.push(' ');
                i += 1;
                if escaped {
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                    continue;
                }
                if chars[i - 1] == quote {
                    break;
                }
            }
            continue;
        }

        // Comment: the rest of the line is dead.
        if chars[i] == '#' {
            break;
        }

        out.push(chars[i]);
        i += 1;
    }

    (out, open_delim)
}

fn starts_with_at(chars: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    i + p.len() <= chars.len() && chars[i..i + p.len()] == p[..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modules(src: &str) -> Vec<String> {
        extract(src).into_iter().map(|i| i.module).collect()
    }

    #[test]
    fn plain_import() {
        assert_eq!(modules("import os\n"), vec!["os"]);
    }

    #[test]
    fn dotted_import_keeps_top_level() {
        assert_eq!(modules("import cv2.aruco\n"), vec!["cv2"]);
        assert_eq!(modules("from os.path import join\n"), vec!["os"]);
    }

    #[test]
    fn aliased_and_multiple() {
        assert_eq!(modules("import numpy as np\n"), vec!["numpy"]);
        assert_eq!(modules("import os, sys\n"), vec!["os", "sys"]);
    }

    #[test]
    fn relative_imports_are_skipped() {
        assert!(modules("from . import helpers\n").is_empty());
        assert!(modules("from .models import User\n").is_empty());
        assert!(modules("from ..pkg import thing\n").is_empty());
    }

    #[test]
    fn indented_imports_are_found() {
        assert_eq!(modules("try:\n    import ujson\n"), vec!["ujson"]);
    }

    #[test]
    fn imports_in_comments_and_strings_are_ignored() {
        assert!(modules("# import requests\n").is_empty());
        assert!(modules("x = \"import requests\"\n").is_empty());
    }

    #[test]
    fn imports_inside_docstrings_are_ignored() {
        let src = "\"\"\"\nExample:\n    import requests\n\"\"\"\nimport os\n";
        assert_eq!(modules(src), vec!["os"]);
    }

    #[test]
    fn position_covers_the_module_path() {
        let refs = extract("import cv2.aruco\n");
        assert_eq!(refs[0].line, 0);
        assert_eq!(refs[0].start, 7);
        assert_eq!(refs[0].end, 16);
    }

    #[test]
    fn second_module_in_a_list_is_positioned() {
        let refs = extract("import os, sys\n");
        assert_eq!(refs[1].module, "sys");
        assert_eq!(refs[1].start, 11);
        assert_eq!(refs[1].end, 14);
    }

    #[test]
    fn not_an_import_statement() {
        assert!(modules("important = 1\n").is_empty());
        assert!(modules("fromage = 2\n").is_empty());
    }
}

#[cfg(test)]
mod notebook_tests {
    use super::*;

    #[test]
    fn notebook_code_cells_are_read_and_other_cells_skipped() {
        let nb = r#"{"cells":[
            {"cell_type":"markdown","source":["import not_code\n"]},
            {"cell_type":"code","source":["%pip install requests\n","import requests\n","import yaml\n"]},
            {"cell_type":"code","source":"import numpy as np"}
        ],"metadata":{"kernelspec":{"name":"python3","language":"python"}}}"#;
        let code = notebook_code(nb).unwrap();
        let found: Vec<String> = extract(&code).into_iter().map(|i| i.module).collect();
        assert_eq!(found, ["requests", "yaml", "numpy"]);
        let r = r#"{"cells":[],"metadata":{"kernelspec":{"language":"R"}}}"#;
        assert!(notebook_code(r).is_none());
        assert!(notebook_code("not json").is_none());
    }

    fn cell_modules(src: &str) -> Vec<(String, u32)> {
        extract(&mask_ipython(src))
            .into_iter()
            .map(|i| (i.module, i.line))
            .collect()
    }

    #[test]
    fn magics_and_shell_escapes_are_not_imports() {
        let src = "%pip install requests\n!pip install flask\n%matplotlib inline\nimport numpy\n";
        assert_eq!(cell_modules(src), vec![("numpy".to_string(), 3)]);
    }

    #[test]
    fn a_quote_in_a_shell_command_does_not_swallow_later_imports() {
        // Unmasked, the `'''` opens a "docstring" that hides `import pandas`.
        assert!(modules_unmasked("!echo '''\nimport pandas\n").is_empty());
        assert_eq!(
            cell_modules("!echo '''\nimport pandas\n"),
            vec![("pandas".to_string(), 1)]
        );
        assert_eq!(
            cell_modules("files = !ls '''\nimport requests\n"),
            vec![("requests".to_string(), 1)]
        );
    }

    fn modules_unmasked(src: &str) -> Vec<String> {
        extract(src).into_iter().map(|i| i.module).collect()
    }

    #[test]
    fn help_requests_are_masked() {
        let masked = mask_ipython("?np\n??np.linalg\nnp.linalg.norm?\nimport os\n");
        assert_eq!(masked.lines().filter(|l| !l.is_empty()).count(), 1);
        assert_eq!(
            cell_modules("?np\nimport os\n"),
            vec![("os".to_string(), 1)]
        );
    }

    #[test]
    fn python_cell_magics_keep_their_body() {
        assert_eq!(
            cell_modules("%%time\nimport numpy\n"),
            vec![("numpy".to_string(), 1)]
        );
    }

    #[test]
    fn foreign_cell_magics_hide_the_whole_cell() {
        assert!(cell_modules("%%writefile helpers.py\nimport requests\n").is_empty());
        assert!(cell_modules("\n%%bash\nimport requests\n").is_empty());
    }

    #[test]
    fn real_python_is_left_alone() {
        let src = "x = a != b\ny = (c\n     % d)\ns = \"why?\"\nz = 1 # really?\n";
        // Only the `% d` continuation looks like a magic, and it holds no import.
        let masked = mask_ipython(src);
        assert!(masked.contains("x = a != b"));
        assert!(masked.contains("s = \"why?\""));
        assert!(masked.contains("z = 1 # really?"));
    }

    #[test]
    fn masked_shell_lines_produce_no_attribute_uses() {
        let src = "import mediapipe as mp\n!python -c mp.solutions\nx = mp.tasks\n";
        let uses = extract_attr_uses(&mask_ipython(src), &["mp".to_string()]);
        let attrs: Vec<String> = uses.into_iter().map(|u| u.attr).collect();
        assert_eq!(attrs, vec!["tasks"]);
    }
}

#[cfg(test)]
mod attr_tests {
    use super::*;

    fn bases() -> Vec<String> {
        vec!["mp".to_string()]
    }

    fn found(src: &str) -> Vec<String> {
        extract_attr_uses(src, &bases())
            .into_iter()
            .map(|u| u.attr)
            .collect()
    }

    #[test]
    fn takes_the_first_attribute_in_a_chain() {
        assert_eq!(found("mp_hands = mp.solutions.hands\n"), vec!["solutions"]);
    }

    #[test]
    fn requires_a_standalone_name() {
        // Substring and attribute-of-something-else must not match.
        assert!(found("mpx.solutions\n").is_empty());
        assert!(found("self.mp.solutions\n").is_empty());
    }

    #[test]
    fn ignores_strings_and_comments() {
        assert!(found("# mp.solutions\n").is_empty());
        assert!(found("s = \"mp.solutions\"\n").is_empty());
    }

    #[test]
    fn ignores_import_statements() {
        assert!(found("import mp.solutions\n").is_empty());
    }

    #[test]
    fn records_position_over_base_and_attribute() {
        let uses = extract_attr_uses("x = mp.solutions\n", &bases());
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].line, 0);
        assert_eq!(uses[0].start, 4);
        assert_eq!(uses[0].end, 16);
    }

    #[test]
    fn binding_follows_the_alias() {
        let refs = extract("import mediapipe as mp\n");
        assert_eq!(refs[0].binding.as_deref(), Some("mp"));

        let plain = extract("import mediapipe\n");
        assert_eq!(plain[0].binding.as_deref(), Some("mediapipe"));

        // Points below the root, so no root binding.
        let dotted = extract("import a.b as c\n");
        assert_eq!(dotted[0].binding, None);
        let from = extract("from a import b\n");
        assert_eq!(from[0].binding, None);
    }
}

#[cfg(test)]
mod symbol_tests {
    use super::*;

    fn targets(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(m, s)| (m.to_string(), s.to_string()))
            .collect()
    }

    fn found(src: &str, pairs: &[(&str, &str)]) -> Vec<(String, u32, bool)> {
        symbol_uses(src, &targets(pairs))
            .into_iter()
            .map(|u| (u.written, u.line, u.from_import))
            .collect()
    }

    #[test]
    fn numpy_aliases_are_followed() {
        let src = "import numpy as np\nx = np.zeros(3, dtype=np.float)\ny = np.float64(1)\n";
        assert_eq!(
            found(src, &[("numpy", "float")]),
            [("np.float".into(), 2, false)]
        );
        let src = "import numpy\nnumpy.float(1)\n# np.float in a comment\ns = 'numpy.float'\n";
        assert_eq!(
            found(src, &[("numpy", "float")]),
            [("numpy.float".into(), 2, false)]
        );
        // Not imported: `np` is somebody else's name.
        assert!(found("np.float\n", &[("numpy", "float")]).is_empty());
        // `self.np.float` is an attribute of something else.
        assert!(found("import numpy as np\nself.np.float\n", &[("numpy", "float")]).is_empty());
    }

    #[test]
    fn from_imports_and_parenthesized_lists() {
        let src = "from pydantic import BaseSettings\n";
        assert_eq!(
            found(src, &[("pydantic", "BaseSettings")]),
            [("from pydantic import BaseSettings".into(), 1, true)]
        );
        let src = "from collections import (\n    OrderedDict,\n    Mapping as M,\n)\n";
        assert_eq!(
            found(src, &[("collections", "Mapping")]),
            [("from collections import Mapping".into(), 1, true)]
        );
        // `collections.abc.Mapping` is the replacement, not a use.
        let src = "import collections\nimport collections.abc\nisinstance(x, collections.abc.Mapping)\nisinstance(x, collections.Mapping)\n";
        assert_eq!(
            found(src, &[("collections", "Mapping")]),
            [("collections.Mapping".into(), 4, false)]
        );
    }

    #[test]
    fn decorators_and_submodule_aliases() {
        let src = "import asyncio\n@asyncio.coroutine\ndef f():\n    yield from asyncio.sleep(0)\n";
        assert_eq!(
            found(src, &[("asyncio", "coroutine")]),
            [("asyncio.coroutine".into(), 2, false)]
        );
        let src = "from scipy import integrate\nintegrate.simps(y)\n";
        assert_eq!(
            found(src, &[("scipy.integrate", "simps")]),
            [("integrate.simps".into(), 2, false)]
        );
        let src =
            "import scipy.integrate as si\nsi.simps(y)\nimport scipy\nscipy.integrate.simps(y)\n";
        assert_eq!(
            found(src, &[("scipy.integrate", "simps")]),
            [
                ("si.simps".into(), 2, false),
                ("scipy.integrate.simps".into(), 4, false)
            ]
        );
    }
}

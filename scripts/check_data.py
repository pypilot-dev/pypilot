#!/usr/bin/env python3
"""Report drift between PyPilot's bundled data tables and upstream.

The tables in helper/data/*.json are compiled into the engine AND re-fetched
by released engines from the `main` branch every `data_refresh_days`, so they
must be both current and schema-compatible with already-released engines.

Checks:
  schema      every table parses and keeps the keys/field types it had at the
              latest git tag (released engines parse these files)
  torch       newest torch releases on PyPI vs frameworks.json
  cuda-index  CUDA wheel indexes on download.pytorch.org vs frameworks.json,
              and which cuXXX builds the newest torch actually ships
  tensorflow  newest tensorflow on PyPI vs frameworks.json
  companions  torchvision / torchaudio: the torch they pin must be in the table
  extensions  xformers / bitsandbytes / flash-attn / mamba-ssm / causal-conv1d:
              wheel platforms and torch pin of the newest release vs
              cuda_extensions.json
  conda-forge every package in conda_forge_packages.json still exists on
              conda-forge and is built for the platforms the table says (via
              prefix.dev); a table older than CONDA_FORGE_MAX_AGE_DAYS is
              stale (regenerate with scripts/gen_conda_forge.py); newer
              conda-forge versions are reported as info
  python      a released CPython newer than the engine's MAX_MINOR (it would
              call every package unsupported there); plus the newest CPython
              the newest torch ships wheels for (informational)
  nvidia      every CUDA version torch builds target needs a driver row in
              nvidia.json; plus a reminder to check NVIDIA's release notes

Exit codes: 0 current, 1 stale (or schema break), 2 network error and nothing
found stale. Stdlib only, Python 3.9+.

Usage: python scripts/check_data.py [--json]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

ROOT = Path(__file__).resolve().parent.parent
DATA_DIR = ROOT / "helper" / "data"
FILES = ["nvidia.json", "frameworks.json", "import_map.json", "cuda_extensions.json",
         "conda_forge_packages.json"]

PYPI = "https://pypi.org/pypi/{}/json"
PYPI_VERSION = "https://pypi.org/pypi/{}/{}/json"
TORCH_INDEX_ROOT = "https://download.pytorch.org/whl/"
TORCH_INDEX_PKG = "https://download.pytorch.org/whl/{}/torch/"
NVIDIA_NOTES = "https://docs.nvidia.com/cuda/cuda-toolkit-release-notes/index.html"
CPYTHON_RELEASES = "https://endoflife.date/api/python.json"
PREFIX_GRAPHQL = "https://prefix.dev/api/graphql"
CONDA_PLATFORMS = ["linux-64", "linux-aarch64", "osx-64", "osx-arm64", "win-64"]
# Per-Python build lists go stale slowly (new CPythons, dropped old ones), and
# checking them costs a query per package, so they are refreshed by age.
CONDA_FORGE_MAX_AGE_DAYS = 90
PYVERSION_RS = ROOT / "helper" / "src" / "pypi" / "pyversion.rs"
OLDEST_CUDA_TAG = 118  # Older indexes are frozen; not worth tracking.

OK, STALE, INFO, ERROR = "ok", "stale", "info", "error"
USER_AGENT = "pypilot-check-data (+https://github.com/pypilot-dev/pypilot)"


class NetworkError(Exception):
    pass


# --------------------------------------------------------------------------
# helpers


def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.read().decode("utf-8", "replace")
    except (urllib.error.URLError, OSError) as exc:
        raise NetworkError(f"{url}: {exc}") from exc


def fetch_json(url: str) -> Any:
    text = fetch(url)
    try:
        return json.loads(text)
    except ValueError as exc:
        raise NetworkError(f"{url}: invalid JSON ({exc})") from exc


_VERSION_RE = re.compile(
    r"^(\d+(?:\.\d+)*)(?:(a|b|rc)(\d+))?(?:\.post(\d+))?(?:\.dev(\d+))?$"
)


def vkey(version: str) -> Tuple:
    """Sort key for the PEP 440 subset these packages use."""
    m = _VERSION_RE.match(version.split("+")[0])
    if not m:
        return ((0,), 0, 0, 0)
    release = tuple(int(p) for p in m.group(1).split("."))
    release = release + (0,) * (4 - len(release))
    pre = {"a": -3, "b": -2, "rc": -1}.get(m.group(2) or "", 0)
    if m.group(5) is not None and pre == 0:
        pre = -4  # .devN sorts before alphas
    return (release, pre, int(m.group(3) or 0), int(m.group(4) or 0))


def is_final(version: str) -> bool:
    m = _VERSION_RE.match(version)
    return bool(m) and m.group(2) is None and m.group(5) is None


def final_releases(pypi: Dict) -> List[str]:
    """Non-prerelease versions that have at least one non-yanked file."""
    out = []
    for ver, files in pypi.get("releases", {}).items():
        if is_final(ver) and any(not f.get("yanked") for f in files):
            out.append(ver)
    return sorted(out, key=vkey)


def wheel_platforms(files: List[Dict]) -> List[str]:
    """Map wheel platform tags to the table's `os-arch` keys."""
    keys = set()
    for f in files:
        name = f.get("filename", "")
        if f.get("yanked") or not name.endswith(".whl"):
            continue
        plat = name[:-4].split("-")[-1]
        for tag in plat.split("."):
            if tag == "any":
                continue
            if tag.startswith(("manylinux", "musllinux", "linux")):
                if tag.endswith("x86_64"):
                    keys.add("linux-x86_64")
                elif tag.endswith("aarch64"):
                    keys.add("linux-aarch64")
            elif tag == "win_amd64":
                keys.add("windows-x86_64")
            elif tag == "win_arm64":
                keys.add("windows-aarch64")
            elif tag.startswith("macosx"):
                if tag.endswith(("arm64", "universal2")):
                    keys.add("macos-aarch64")
                if tag.endswith(("x86_64", "universal2", "intel")):
                    keys.add("macos-x86_64")
    return sorted(keys)


def torch_requirement(requires_dist: Optional[List[str]]) -> Optional[str]:
    """The specifier a package declares on torch, e.g. '==2.9.0'."""
    for req in requires_dist or []:
        req = req.split(";")[0].strip()
        m = re.match(r"^torch\s*\(?\s*([^)]*)\)?$", req)
        if m:
            return m.group(1).replace(" ", "") or None
    return None


def cuda_tag_to_version(tag: str) -> str:
    digits = tag[2:]
    return f"{digits[:-1]}.{digits[-1]}"


def load_tables() -> Tuple[Dict[str, Any], List[str]]:
    tables, errors = {}, []
    for name in FILES:
        path = DATA_DIR / name
        try:
            tables[name] = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            errors.append(f"{name}: {exc}")
    return tables, errors


# --------------------------------------------------------------------------
# checks: each returns a list of result dicts


def result(check: str, status: str, message: str, **details: Any) -> Dict:
    return {"check": check, "status": status, "message": message, "details": details}


def json_type(value: Any) -> str:
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "float"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return "null"


def compare_shape(old: Any, new: Any, path: str, problems: List[str]) -> None:
    """Flag removed keys and changed types; additions are always allowed."""
    if json_type(old) != json_type(new):
        problems.append(f"{path}: type changed {json_type(old)} -> {json_type(new)}")
        return
    if isinstance(old, dict):
        for key, old_val in old.items():
            if key not in new:
                problems.append(f"{path}.{key}: removed")
            else:
                compare_shape(old_val, new[key], f"{path}.{key}", problems)
    elif isinstance(old, list) and old and new:
        if all(isinstance(e, dict) for e in old):
            required = set.intersection(*(set(e) for e in old))
            sample: Dict[str, Any] = {}
            for e in old:
                for k, v in e.items():
                    sample.setdefault(k, v)
            for i, elem in enumerate(new):
                where = f"{path}[{i}]"
                if not isinstance(elem, dict):
                    problems.append(f"{where}: type changed object -> {json_type(elem)}")
                    continue
                for k in sorted(required - set(elem)):
                    problems.append(f"{where}.{k}: required field missing")
                for k, v in elem.items():
                    if k in sample:
                        compare_shape(sample[k], v, f"{where}.{k}", problems)
        else:
            for i, elem in enumerate(new):
                compare_shape(old[0], elem, f"{path}[{i}]", problems)


def check_schema(tables: Dict[str, Any], parse_errors: List[str]) -> List[Dict]:
    out = [result("schema", STALE, f"does not parse: {e}") for e in parse_errors]
    try:
        tag = subprocess.run(
            ["git", "describe", "--tags", "--abbrev=0"],
            cwd=ROOT, capture_output=True, text=True, check=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        out.append(result("schema", INFO,
                          "no git tag found; skipped compatibility check against the last release"))
        return out
    for name, new in tables.items():
        try:
            old_text = subprocess.run(
                ["git", "show", f"{tag}:helper/data/{name}"],
                cwd=ROOT, capture_output=True, text=True, check=True, encoding="utf-8",
            ).stdout
        except (OSError, subprocess.CalledProcessError):
            out.append(result("schema", INFO, f"{name}: not present at {tag}; nothing to compare"))
            continue
        problems: List[str] = []
        compare_shape(json.loads(old_text), new, name, problems)
        if problems:
            out.append(result("schema", STALE,
                              f"{name}: incompatible with engines released at {tag} "
                              f"({len(problems)} problem(s))", problems=problems))
        else:
            out.append(result("schema", OK, f"{name}: parses and is compatible with {tag}"))
    return out


def check_torch(fw: Dict, torch_pypi: Dict) -> List[Dict]:
    known = {r["version"] for r in fw["torch"]["releases"]}
    newest_known = max(known, key=vkey)
    newer = [v for v in final_releases(torch_pypi) if vkey(v) > vkey(newest_known)]
    latest = torch_pypi["info"]["version"]
    if newer:
        return [result("torch", STALE,
                       f"PyPI has torch {latest}; table stops at {newest_known}. "
                       f"Missing: {', '.join(newer)}", missing=newer, latest=latest)]
    return [result("torch", OK, f"torch {latest} is in the table", latest=latest)]


def check_cuda_indexes(fw: Dict, latest_torch: str) -> List[Dict]:
    out = []
    page = fetch(TORCH_INDEX_ROOT)
    published = sorted({t for t in re.findall(r"\bcu(\d{3})\b", page)
                        if int(t) >= OLDEST_CUDA_TAG}, key=int)
    published = [f"cu{t}" for t in published]
    in_table = sorted({b for r in fw["torch"]["releases"] for b in r["cuda_builds"]})

    # Stable torch versions each index carries; an index can exist before any
    # stable release lands on it, which is not yet something to table.
    carried: Dict[str, List[str]] = {}
    for tag in published:
        text = fetch(TORCH_INDEX_PKG.format(tag))
        found = set(re.findall(r"torch-([0-9][0-9a-z.]*)(?:%2B|\+)" + tag, text))
        carried[tag] = sorted((v for v in found if is_final(v)), key=vkey)

    new_tags = [t for t in published if t not in in_table]
    stale_tags = [t for t in new_tags if carried[t]]
    empty_tags = [t for t in new_tags if not carried[t]]
    if stale_tags:
        out.append(result("cuda-index", STALE,
                          f"download.pytorch.org has stable torch on {', '.join(stale_tags)}, "
                          f"which no table row uses",
                          published=published, missing=stale_tags))
    else:
        out.append(result("cuda-index", OK,
                          f"every index with stable torch ({', '.join(published)}) is tabled"
                          if not empty_tags else
                          "every index with stable torch is tabled", published=published))
    if empty_tags:
        out.append(result("cuda-index", INFO,
                          f"{', '.join(empty_tags)} exist but carry no stable torch yet (not tabled until they do)",
                          empty=empty_tags))

    # Which of those indexes actually carry the newest torch.
    builds = [t for t in published if latest_torch in carried[t]]
    row = next((r for r in fw["torch"]["releases"] if r["version"] == latest_torch), None)
    if row is None:
        out.append(result("cuda-index", STALE,
                          f"torch {latest_torch} ships {', '.join(builds) or 'no CUDA builds'}; "
                          f"add a row: {json.dumps({'version': latest_torch, 'cuda_builds': builds})}",
                          version=latest_torch, cuda_builds=builds))
    elif sorted(row["cuda_builds"]) != builds:
        out.append(result("cuda-index", STALE,
                          f"torch {latest_torch}: table says {row['cuda_builds']}, "
                          f"index has {builds}", version=latest_torch, cuda_builds=builds))
    else:
        out.append(result("cuda-index", OK, f"torch {latest_torch} builds match: {builds}"))
    return out


def check_tensorflow(fw: Dict, tf_pypi: Dict) -> List[Dict]:
    known = [r["version"] for r in fw["tensorflow"]["releases"]]
    newest_known = max(known, key=vkey)
    # min_cuda only changes per minor release; compare on major.minor.
    minors = {vkey(v)[0][:2] for v in known}
    newer = [v for v in final_releases(tf_pypi)
             if vkey(v) > vkey(newest_known) and vkey(v)[0][:2] not in minors]
    latest = tf_pypi["info"]["version"]
    if newer:
        return [result("tensorflow", STALE,
                       f"PyPI has tensorflow {latest}; table stops at {newest_known}. "
                       f"Missing minor releases: {', '.join(newer)} "
                       f"(min_cuda from https://www.tensorflow.org/install/source#gpu)",
                       missing=newer, latest=latest)]
    return [result("tensorflow", OK,
                   f"tensorflow {latest} is covered (table has {newest_known})", latest=latest)]


def check_companions(fw: Dict) -> List[Dict]:
    out = []
    known = {r["version"] for r in fw["torch"]["releases"]}
    for pkg in ("torchvision", "torchaudio"):
        latest = fetch_json(PYPI.format(pkg))["info"]["version"]
        info = fetch_json(PYPI_VERSION.format(pkg, latest))["info"]
        spec = torch_requirement(info.get("requires_dist"))
        m = re.match(r"^(==|>=|~=)([0-9][0-9.]*)", spec or "")
        floor = m.group(2) if m else None
        if floor and floor not in known and vkey(floor) > vkey(max(known, key=vkey)):
            out.append(result("companions", STALE,
                              f"{pkg} {latest} requires torch{spec}, newer than frameworks.json knows",
                              package=pkg, latest=latest, torch=spec))
        else:
            out.append(result("companions", OK,
                              f"{pkg} {latest} (torch{spec or ' unpinned'})",
                              package=pkg, latest=latest, torch=spec))
    return out


def check_extensions(ext_table: Dict) -> List[Dict]:
    out = []
    for pkg in ext_table["packages"]:
        name = pkg["name"]
        data = fetch_json(PYPI.format(name))
        latest = data["info"]["version"]
        rows = sorted(pkg["releases"], key=lambda r: vkey(r["version"]))
        row = None
        for r in rows:
            if vkey(r["version"]) <= vkey(latest):
                row = r
        platforms = wheel_platforms(data.get("releases", {}).get(latest, []))
        problems = []
        if row is None:
            problems.append("no row covers it")
        else:
            if sorted(row.get("platforms", [])) != platforms:
                problems.append(f"wheel platforms {platforms or '[] (sdist only)'} "
                                f"vs table row {row['version']}: {sorted(row.get('platforms', []))}")
            if "torch" in row:
                spec = torch_requirement(
                    fetch_json(PYPI_VERSION.format(name, latest))["info"].get("requires_dist"))
                if spec != row["torch"]:
                    problems.append(f"requires torch{spec} vs table row "
                                    f"{row['version']}: torch{row['torch']}")
        if problems:
            out.append(result("extensions", STALE, f"{name} {latest}: " + "; ".join(problems),
                              package=name, latest=latest, platforms=platforms))
        else:
            out.append(result("extensions", OK,
                              f"{name} {latest} matches row {row['version']}",
                              package=name, latest=latest))
    return out


def conda_forge_latest(names: List[str]) -> Dict[str, Optional[Dict]]:
    """conda name -> {version, platforms}, or None when conda-forge lacks it."""
    out: Dict[str, Optional[Dict]] = {}
    for i in range(0, len(names), 20):
        chunk = names[i:i + 20]
        fields = " ".join(
            f'p{j}: package(channelName: "conda-forge", name: {json.dumps(n)}) '
            f"{{ latestVersion {{ version platforms }} }}"
            for j, n in enumerate(chunk))
        body = json.dumps({"query": "{ " + fields + " }"}).encode()
        req = urllib.request.Request(PREFIX_GRAPHQL, data=body, headers={
            "User-Agent": USER_AGENT, "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                data = json.loads(resp.read().decode("utf-8"))
        except (urllib.error.URLError, OSError, ValueError) as exc:
            raise NetworkError(f"{PREFIX_GRAPHQL}: {exc}") from exc
        if data.get("errors"):
            raise NetworkError(f"{PREFIX_GRAPHQL}: {data['errors']}")
        for j, n in enumerate(chunk):
            pkg = data["data"].get(f"p{j}")
            out[n] = pkg["latestVersion"] if pkg else None
    return out


def tabled_platforms(row: Dict) -> List[str]:
    return sorted(b["platform"] for b in row["builds"])


def current_platforms(row: Dict, info: Dict) -> List[str]:
    if tabled_platforms(row) == ["noarch"] and "noarch" in info["platforms"]:
        return ["noarch"]
    return sorted(p for p in info["platforms"] if p in CONDA_PLATFORMS)


def platforms_match(row: Dict, info: Dict) -> bool:
    return current_platforms(row, info) == tabled_platforms(row)


def check_conda_forge(table: Dict) -> List[Dict]:
    import datetime
    out = []
    generated = datetime.date.fromisoformat(table["generated"])
    age = (datetime.date.today() - generated).days
    regen = "regenerate with `python scripts/gen_conda_forge.py`"
    if age > CONDA_FORGE_MAX_AGE_DAYS:
        out.append(result("conda-forge", STALE,
                          f"conda_forge_packages.json was generated {age} days ago "
                          f"(limit {CONDA_FORGE_MAX_AGE_DAYS}); {regen}", age_days=age))
    rows = table["packages"]
    latest = conda_forge_latest(sorted({r["conda"] for r in rows}))
    # The batched API answer has been seen to under-report platforms under
    # load, so a row that looks changed is asked about again on its own.
    def recheck(name: str) -> Optional[Dict]:
        return conda_forge_latest([name]).get(name)

    gone, moved, newer = [], [], []
    for r in rows:
        info = latest.get(r["conda"])
        if info is None or not platforms_match(r, info):
            info = recheck(r["conda"])
        if info is None:
            gone.append(r["pypi"])
            continue
        if not platforms_match(r, info):
            moved.append(f"{r['pypi']}: {tabled_platforms(r)} -> {current_platforms(r, info)}")
        if vkey(info["version"]) > vkey(r["version"]):
            newer.append(f"{r['pypi']} {r['version']} -> {info['version']}")
    if gone:
        out.append(result("conda-forge", STALE,
                          f"no longer on conda-forge: {', '.join(gone)}; {regen}", missing=gone))
    if moved:
        out.append(result("conda-forge", STALE,
                          f"{len(moved)} package(s) changed platforms; {regen}", problems=moved))
    if not gone and not moved:
        out.append(result("conda-forge", OK,
                          f"{len(rows)} packages exist with the tabled platforms "
                          f"(table {age} days old)"))
    if newer:
        out.append(result("conda-forge", INFO,
                          f"{len(newer)} package(s) have newer conda-forge releases "
                          f"(versions are informational; refreshed on regeneration)",
                          problems=newer))
    return out


def check_cpython() -> List[Dict]:
    m = re.search(r"pub const MAX_MINOR: u8 = (\d+);", PYVERSION_RS.read_text(encoding="utf-8"))
    if not m:
        return [result("python", STALE, f"MAX_MINOR not found in {PYVERSION_RS.name}")]
    ceiling = int(m.group(1))
    released = [int(c["cycle"].split(".")[1]) for c in fetch_json(CPYTHON_RELEASES)
                if c.get("cycle", "").startswith("3.")]
    newest = max(released)
    if newest > ceiling:
        return [result("python", STALE,
                       f"CPython 3.{newest} is released but MAX_MINOR in "
                       f"helper/src/pypi/pyversion.rs is {ceiling}: the engine treats every "
                       f"package as unsupported on 3.{newest}. Raise it",
                       newest=f"3.{newest}", max_minor=ceiling)]
    return [result("python", OK, f"MAX_MINOR covers the newest CPython (3.{newest})")]


def check_python(torch_pypi: Dict) -> List[Dict]:
    latest = torch_pypi["info"]["version"]
    tags = set()
    for f in torch_pypi.get("releases", {}).get(latest, []):
        m = re.search(r"-cp3(\d+)t?-", f.get("filename", ""))
        if m:
            tags.add(int(m.group(1)))
    if not tags:
        return [result("python", INFO, f"torch {latest}: no CPython wheels found on PyPI")]
    newest = f"3.{max(tags)}"
    return [result("python", INFO,
                   f"torch {latest} has wheels for Python 3.{min(tags)}-{newest}. "
                   f"Python support is derived from PyPI at runtime (not tabled); "
                   f"make sure fixtures/tests do not assume an older ceiling",
                   newest_python=newest)]


def check_nvidia(nvidia: Dict, fw: Dict) -> List[Dict]:
    rows = {r["cuda"] for r in nvidia["drivers"]}
    newest = max(rows, key=vkey)
    out = []
    targets = sorted({cuda_tag_to_version(b) for r in fw["torch"]["releases"]
                      for b in r["cuda_builds"]}
                     | {r["min_cuda"] for r in fw["tensorflow"]["releases"]}, key=vkey)
    missing = [v for v in targets if v not in rows]
    if missing:
        out.append(result("nvidia", STALE,
                          f"frameworks.json targets CUDA {', '.join(missing)} but nvidia.json "
                          f"has no driver row for it (newest row: CUDA {newest})",
                          missing=missing, newest=newest))
    else:
        out.append(result("nvidia", OK, "every CUDA version frameworks target has a driver row"))
    out.append(result("nvidia", INFO,
                      f"driver table can't be scraped; newest row is CUDA {newest}. "
                      f"Check {NVIDIA_NOTES} (Table 3) for newer GA releases",
                      newest=newest, source=NVIDIA_NOTES))
    return out


# --------------------------------------------------------------------------


def run_checks() -> List[Dict]:
    tables, parse_errors = load_tables()
    results = check_schema(tables, parse_errors)
    fw = tables.get("frameworks.json")
    nvidia = tables.get("nvidia.json")
    ext = tables.get("cuda_extensions.json")
    conda = tables.get("conda_forge_packages.json")

    def guarded(name: str, fn, *args) -> None:
        try:
            results.extend(fn(*args))
        except NetworkError as exc:
            results.append(result(name, ERROR, f"network error: {exc}"))
        except (KeyError, TypeError, ValueError) as exc:
            results.append(result(name, STALE, f"unexpected table structure: {exc!r}"))

    torch_pypi: Optional[Dict] = None
    try:
        torch_pypi = fetch_json(PYPI.format("torch"))
    except NetworkError as exc:
        results.append(result("torch", ERROR, f"network error: {exc}"))

    if fw is not None:
        if torch_pypi is not None:
            guarded("torch", check_torch, fw, torch_pypi)
            guarded("cuda-index", check_cuda_indexes, fw, torch_pypi["info"]["version"])
        guarded("tensorflow", lambda f: check_tensorflow(f, fetch_json(PYPI.format("tensorflow"))), fw)
        guarded("companions", check_companions, fw)
    if ext is not None:
        guarded("extensions", check_extensions, ext)
    if conda is not None:
        guarded("conda-forge", check_conda_forge, conda)
    guarded("python", check_cpython)
    if torch_pypi is not None:
        guarded("python", check_python, torch_pypi)
    if nvidia is not None and fw is not None:
        guarded("nvidia", check_nvidia, nvidia, fw)
    return results


def exit_code(results: List[Dict]) -> int:
    statuses = {r["status"] for r in results}
    if STALE in statuses:
        return 1
    if ERROR in statuses:
        return 2
    return 0


def print_human(results: List[Dict], code: int) -> None:
    labels = {OK: "  ok   ", STALE: "STALE  ", INFO: " info  ", ERROR: "ERROR  "}
    current = None
    for r in results:
        if r["check"] != current:
            current = r["check"]
            print(f"\n[{current}]")
        print(f"{labels[r['status']]}{r['message']}")
        for p in r["details"].get("problems", [])[:20]:
            print(f"         - {p}")
    print()
    if code == 0:
        print("Data tables are current.")
    elif code == 1:
        n = sum(r["status"] == STALE for r in results)
        print(f"{n} stale item(s). Update helper/data/*.json first (see AGENTS.md), "
              f"commit as `data: ...`, then continue.")
    else:
        print("Could not reach upstream for some checks; nothing found stale in the rest.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args()
    results = run_checks()
    code = exit_code(results)
    if args.json:
        status = {0: "current", 1: "stale", 2: "network-error"}[code]
        print(json.dumps({"status": status, "exit_code": code, "results": results}, indent=2))
    else:
        print_human(results, code)
    return code


if __name__ == "__main__":
    sys.exit(main())

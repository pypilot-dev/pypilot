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
              prefix.dev; a platform counts when one of the three newest
              versions is built for it, so a release still uploading is not
              drift); a table older than CONDA_FORGE_MAX_AGE_DAYS is
              stale (regenerate with scripts/gen_conda_forge.py); newer
              conda-forge versions are reported as info
  python      a released CPython newer than the engine's MAX_MINOR (it would
              call every package unsupported there); plus the newest CPython
              the newest torch ships wheels for (informational)
  nvidia      every CUDA version torch builds target needs a driver row in
              nvidia.json; plus a reminder to check NVIDIA's release notes
  breakages   known_breakages.json: every row has its required fields with
              the right types, ids are unique, versions and Pythons parse,
              and (online) every package, fixed_in release and replacement
              it names exists on PyPI

Exit codes: 0 current, 1 stale (or schema break), 2 network error and nothing
found stale. Stdlib only, Python 3.9+.

Usage: python scripts/check_data.py [--json] [--self-test]
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
         "conda_forge_packages.json", "known_breakages.json"]

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


# Same rule as scripts/gen_conda_forge.py: a platform counts as built when one
# of the newest CONDA_RECENT_VERSIONS versions has a build for it. conda-forge
# uploads a release platform by platform; until the last one lands, pixi
# installs the previous release there, so the table keeps that platform.
CONDA_RECENT_VERSIONS = 3
CONDA_VERSIONS_FETCHED = 10


def _conda_vpart(part: str) -> Tuple[int, Any]:
    if part.isdigit():
        return (2, int(part))
    if part == "post":
        return (3, 0)
    if part == "dev":
        return (0, 0)
    return (1, part)


def conda_vkey(version: str) -> Tuple:
    """Sort key for conda versions (numbers as numbers, letters before them)."""
    parts = re.findall(r"\d+|[a-zA-Z]+", version.lower().split("+")[0])
    return tuple(_conda_vpart(p) for p in parts) + ((2, -1),)


def conda_summary(versions: List[Dict]) -> Optional[Dict]:
    """{version, platforms, newest_platforms} from [{version, platforms}]:
    the newest version, the tracked platforms any of the newest
    CONDA_RECENT_VERSIONS is built for, and the newest version's own list
    (for noarch). None when there are no versions."""
    recent = sorted(versions, key=lambda v: conda_vkey(v["version"]),
                    reverse=True)[:CONDA_RECENT_VERSIONS]
    if not recent:
        return None
    plats = {p for v in recent for p in (v.get("platforms") or []) if p in CONDA_PLATFORMS}
    return {"version": recent[0]["version"], "platforms": sorted(plats),
            "newest_platforms": list(recent[0].get("platforms") or []),
            "versions": [v["version"] for v in recent]}


def prefix_query(query: str) -> Dict:
    body = json.dumps({"query": query}).encode()
    req = urllib.request.Request(PREFIX_GRAPHQL, data=body, headers={
        "User-Agent": USER_AGENT, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError) as exc:
        raise NetworkError(f"{PREFIX_GRAPHQL}: {exc}") from exc
    if data.get("errors"):
        raise NetworkError(f"{PREFIX_GRAPHQL}: {data['errors']}")
    return data["data"]


def conda_built_on(name: str, versions: List[str], platforms: List[str]) -> List[str]:
    """The `platforms` one of `versions` has a build for, asked of the
    variants list (authoritative; the per-version `platforms` field has been
    seen to collapse to a single platform)."""
    pairs = [(v, p) for p in platforms for v in versions]
    if not pairs:
        return []
    fields = " ".join(
        f"v{i}: variants(version: {json.dumps(v)}, platform: {json.dumps(p)}, limit: 1) "
        f"{{ page {{ platform }} }}"
        for i, (v, p) in enumerate(pairs))
    pkg = prefix_query(f'{{ package(channelName: "conda-forge", name: {json.dumps(name)}) '
                       f"{{ {fields} }} }}").get("package") or {}
    return sorted({p for i, (_v, p) in enumerate(pairs)
                   if ((pkg.get(f"v{i}") or {}).get("page") or [])})


def conda_forge_latest(names: List[str]) -> Dict[str, Optional[Dict]]:
    """conda name -> conda_summary(), or None when conda-forge lacks it."""
    out: Dict[str, Optional[Dict]] = {}
    for i in range(0, len(names), 10):
        chunk = names[i:i + 10]
        fields = " ".join(
            f'p{j}: package(channelName: "conda-forge", name: {json.dumps(n)}) '
            f"{{ versions(limit: {CONDA_VERSIONS_FETCHED}, page: 0) {{ page {{ version platforms }} }} }}"
            for j, n in enumerate(chunk))
        data = prefix_query("{ " + fields + " }")
        for j, n in enumerate(chunk):
            pkg = data.get(f"p{j}")
            page = ((pkg or {}).get("versions") or {}).get("page") or []
            out[n] = conda_summary(page) if pkg else None
    return out


def tabled_platforms(row: Dict) -> List[str]:
    return sorted(b["platform"] for b in row["builds"])


def current_platforms(row: Dict, info: Dict) -> List[str]:
    if tabled_platforms(row) == ["noarch"] and "noarch" in info["newest_platforms"]:
        return ["noarch"]
    return list(info["platforms"])


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
    # The per-version platform lists have been seen to under-report (down to
    # a single platform, the same wrong answer on every retry), so a tabled
    # platform that looks gone is confirmed against the variants list.
    def recheck(r: Dict, info: Optional[Dict]) -> Optional[Dict]:
        info = info or conda_forge_latest([r["conda"]]).get(r["conda"])
        if info is None or tabled_platforms(r) == ["noarch"]:
            return info
        gone = [p for p in tabled_platforms(r) if p not in info["platforms"]]
        found = conda_built_on(r["conda"], info["versions"], gone)
        return dict(info, platforms=sorted(set(info["platforms"]) | set(found)))

    gone, moved, newer = [], [], []
    for r in rows:
        info = latest.get(r["conda"])
        if info is None or not platforms_match(r, info):
            info = recheck(r, info)
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
# known_breakages.json

# Required fields per section and their JSON types. The engine skips a row
# missing one of these, so a row that fails here is a row that does nothing.
BREAKAGE_SCHEMA: Dict[str, Dict[str, str]] = {
    "api_removals": {"id": "string", "kind": "string", "package": "string",
                     "fixed_in": "string", "dependency": "string",
                     "breaks_from": "string", "constraint": "string",
                     "symptom": "string", "source": "string"},
    "python_removals": {"id": "string", "package": "string", "python_min": "string",
                        "symptom": "string", "source": "string"},
    "shadowing": {"id": "string", "kind": "string", "dist": "string", "module": "string",
                  "symptom": "string", "source": "string"},
    "module_removals": {"id": "string", "module": "string", "dependency": "string",
                        "breaks_from": "string", "constraint": "string",
                        "symptom": "string", "source": "string"},
    "stdlib_removals": {"module": "string", "removed_in": "string", "source": "string"},
    "symbol_removals": {"id": "string", "module": "string", "symbol": "string",
                        "replacement": "string", "source": "string"},
}
BREAKAGE_OPTIONAL: Dict[str, str] = {
    "fixed_in": "string", "python_min": "string", "python_below": "string",
    "prefer": "string", "replacement": "string", "drop_in": "bool", "other": "string",
    "keep": "string", "stdlib_since": "string", "severity": "string", "migrate": "string",
    "removed_in": "string", "dependency": "string", "breaks_from": "string",
    "constraint": "string", "add": "string", "symptom": "string",
    "calls": "array", "calls_in": "array",
}
BREAKAGE_KINDS = {"api_removals": {"abi", "api"},
                  "shadowing": {"clash", "backport", "deprecated"}}
_PYTHON_RE = re.compile(r"^3\.\d+$")
_VERSION_FIELDS = ("fixed_in", "breaks_from")
_PYTHON_FIELDS = ("python_min", "python_below", "stdlib_since", "removed_in")
# A `calls` pattern, as helper/src/core/imports.rs (CallPattern::parse) reads
# it: `engine.execute(`, `.engine.execute(`, `*.append(`, `conn.execute("`,
# `*.append(ignore_index=`. A bare `name(` would match every function call.
_IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
_CALL_RE = re.compile(
    rf"^(?:\.|\*\.|\.\*\.)?{_IDENT}(?:\.{_IDENT})*\((?:|\"|'|{_IDENT}=)$")
_MODULE_RE = re.compile(rf"^{_IDENT}$")


def call_problems(where: str, row: Dict) -> List[str]:
    """Problems with a symbol_removals row's `calls` and `calls_in`."""
    out: List[str] = []
    calls = row.get("calls")
    if calls is None:
        if "calls_in" in row:
            out.append(f"{where}.calls_in: only means something next to calls")
        return out
    if not calls:
        out.append(f"{where}.calls: empty")
    for c in calls if isinstance(calls, list) else []:
        bare = isinstance(c, str) and not c.startswith((".", "*")) and "." not in c.split("(")[0]
        if not isinstance(c, str) or not _CALL_RE.match(c) or bare:
            out.append(f"{where}.calls: {c!r} is not a call pattern")
    for m in row.get("calls_in", []) if isinstance(row.get("calls_in"), list) else []:
        if not isinstance(m, str) or not _MODULE_RE.match(m):
            out.append(f"{where}.calls_in: {m!r} is not a top-level module name")
    # The default symptom is a module AttributeError, wrong for a method.
    if "symptom" not in row:
        out.append(f"{where}.symptom: a calls row says how the call fails")
    # Engines without `calls` look the symbol up as a module attribute; a
    # dotted symbol never matches one, so they stay silent.
    if "." not in str(row.get("symbol", "")):
        out.append(f"{where}.symbol: a calls row names `Class.method`, with a dot")
    if "dependency" not in row:
        out.append(f"{where}: a calls row is a distribution removal")
    return out


def breakage_problems(table: Dict) -> List[str]:
    """Structural problems in known_breakages.json (offline)."""
    problems: List[str] = []
    ids: set = set()
    for section, required in BREAKAGE_SCHEMA.items():
        rows = table.get(section)
        if not isinstance(rows, list):
            problems.append(f"{section}: missing or not an array")
            continue
        for i, row in enumerate(rows):
            where = f"{section}[{i}]"
            if not isinstance(row, dict):
                problems.append(f"{where}: not an object")
                continue
            for key, kind in required.items():
                if key not in row:
                    problems.append(f"{where}.{key}: required field missing")
                elif json_type(row[key]) != kind:
                    problems.append(f"{where}.{key}: expected {kind}, got {json_type(row[key])}")
            for key, kind in BREAKAGE_OPTIONAL.items():
                if key in row and key not in required and json_type(row[key]) != kind:
                    problems.append(f"{where}.{key}: expected {kind}, got {json_type(row[key])}")
            if section in BREAKAGE_KINDS and row.get("kind") not in BREAKAGE_KINDS[section]:
                problems.append(f"{where}.kind: {row.get('kind')!r} is not one of "
                                f"{sorted(BREAKAGE_KINDS[section])}")
            if section == "shadowing":
                if row.get("kind") == "clash" and "other" not in row:
                    problems.append(f"{where}.other: a clash names the other dist")
                if row.get("kind") == "backport" and "stdlib_since" not in row:
                    problems.append(f"{where}.stdlib_since: a backport names its Python")
                if row.get("kind") == "deprecated" and "keep" not in row:
                    problems.append(f"{where}.keep: a deprecated dist names its replacement")
            if section == "python_removals" and "breaks_from" in row:
                if "fixed_in" not in row:
                    problems.append(f"{where}.breaks_from: needs fixed_in, the end of the range")
                elif vkey(row["breaks_from"]) >= vkey(row["fixed_in"]):
                    problems.append(f"{where}.breaks_from: not below fixed_in")
            if section == "symbol_removals":
                problems.extend(call_problems(where, row))
                by_dist = all(k in row for k in ("dependency", "breaks_from", "constraint"))
                if by_dist == ("removed_in" in row):
                    problems.append(f"{where}: needs either removed_in (a Python) or "
                                    "dependency, breaks_from and constraint (a distribution)")
            for key in _VERSION_FIELDS:
                value = row.get(key)
                if isinstance(value, str) and not _VERSION_RE.match(value):
                    problems.append(f"{where}.{key}: {value!r} is not a version")
            for key in _PYTHON_FIELDS:
                value = row.get(key)
                if isinstance(value, str) and not _PYTHON_RE.match(value):
                    problems.append(f"{where}.{key}: {value!r} is not a \"3.N\" Python")
            if row.get("severity") not in (None, "info", "warning", "error"):
                problems.append(f"{where}.severity: {row.get('severity')!r}")
            key = row.get("id") or row.get("module")
            if key in ids:
                problems.append(f"{where}: duplicate id {key!r}")
            ids.add(key)
    return problems


def breakage_names(table: Dict) -> Dict[str, set]:
    """Every PyPI name the table names, with the releases it must have."""
    wanted: Dict[str, set] = {}
    for row in table.get("api_removals", []) + table.get("python_removals", []):
        wanted.setdefault(row["package"], set())
        if "fixed_in" in row:
            wanted[row["package"]].add(row["fixed_in"])
        if "breaks_from" in row and row in table.get("python_removals", []):
            wanted[row["package"]].add(row["breaks_from"])
        for key in ("dependency", "replacement"):
            if key in row:
                wanted.setdefault(row[key], set())
    for row in table.get("shadowing", []):
        for key in ("dist", "other", "keep"):
            if key in row:
                wanted.setdefault(row[key], set())
    for row in table.get("module_removals", []) + table.get("stdlib_removals", []):
        for key in ("dependency", "replacement"):
            if key in row:
                wanted.setdefault(row[key], set())
    # A symbol row's replacement is code, not a distribution.
    for row in table.get("symbol_removals", []):
        for key in ("dependency", "add"):
            if key in row:
                wanted.setdefault(row[key], set())
    return wanted


def check_breakages(table: Dict) -> List[Dict]:
    problems = breakage_problems(table)
    if problems:
        return [result("breakages", STALE,
                       f"known_breakages.json: {len(problems)} structural problem(s)",
                       problems=problems)]
    out = [result("breakages", OK, "known_breakages.json: every row is well formed")]
    # Every name a fix would install or compare against must exist, and every
    # fixed_in must be a real release, or the advice points at nothing.
    wanted = breakage_names(table)
    missing: List[str] = []
    for name in sorted(wanted):
        try:
            releases = fetch_json(PYPI.format(name)).get("releases", {})
        except NetworkError as exc:
            if "404" in str(exc):
                missing.append(f"{name}: not on PyPI")
                continue
            raise
        for version in sorted(wanted[name]):
            if version not in releases:
                missing.append(f"{name} {version}: no such release")
    if missing:
        out.append(result("breakages", STALE,
                          f"known_breakages.json names {len(missing)} thing(s) PyPI does not have",
                          problems=missing))
    else:
        out.append(result("breakages", OK,
                          f"{len(wanted)} packages and their fixed_in releases exist on PyPI"))
    return out


# --------------------------------------------------------------------------


def run_checks() -> List[Dict]:
    tables, parse_errors = load_tables()
    results = check_schema(tables, parse_errors)
    fw = tables.get("frameworks.json")
    nvidia = tables.get("nvidia.json")
    ext = tables.get("cuda_extensions.json")
    conda = tables.get("conda_forge_packages.json")
    breakages = tables.get("known_breakages.json")

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
    if breakages is not None:
        guarded("breakages", check_breakages, breakages)
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


def self_test() -> int:
    """Offline checks of the pure helpers. Exit 0 on success."""
    def row(platforms: List[str]) -> Dict:
        return {"builds": [{"platform": p, "python": []} for p in platforms]}

    # lap 0.5.14 mid-upload: linux-64 still comes from 0.5.13, so no drift.
    lap = conda_summary([
        {"version": "0.5.12", "platforms": ["linux-64", "osx-64", "win-64"]},
        {"version": "0.5.14", "platforms": ["linux-aarch64", "osx-64", "osx-arm64", "win-64"]},
        {"version": "0.5.13", "platforms": ["linux-64", "linux-aarch64", "linux-ppc64le",
                                            "osx-64", "osx-arm64", "win-64"]},
    ])
    assert lap is not None and lap["version"] == "0.5.14", lap
    assert platforms_match(row(CONDA_PLATFORMS), lap), lap
    # cylp 0.94.1 gains osx-arm64 and win-64: still reported.
    cylp = conda_summary([
        {"version": "0.94.1", "platforms": ["linux-aarch64", "osx-64", "osx-arm64", "win-64"]},
        {"version": "0.92.3", "platforms": ["linux-64", "linux-aarch64", "osx-64"]},
    ])
    assert not platforms_match(row(["linux-64", "linux-aarch64", "osx-64"]), cylp), cylp
    assert platforms_match(row(CONDA_PLATFORMS), cylp), cylp
    # Only the three newest versions count; numeric, not string, order.
    old = conda_summary([
        {"version": "1.10", "platforms": ["osx-64"]},
        {"version": "1.9", "platforms": ["osx-64"]},
        {"version": "1.8", "platforms": ["osx-64"]},
        {"version": "1.2", "platforms": ["win-64"]},
    ])
    assert old is not None and old["version"] == "1.10" and old["platforms"] == ["osx-64"], old
    assert conda_vkey("1.0rc1") < conda_vkey("1.0") < conda_vkey("1.0.post1"), "vkey"
    # noarch rows compare against the newest version only.
    pure = conda_summary([{"version": "2", "platforms": ["noarch"]},
                          {"version": "1", "platforms": ["linux-64"]}])
    assert platforms_match(row(["noarch"]), pure), pure
    assert conda_summary([]) is None
    # A new optional field passes the schema check; removals and retypes do not.
    old_t = {"packages": [{"version": "1", "builds": [{"platform": "linux-64", "python": []}]}]}
    new_t = {"packages": [{"version": "2", "builds": [
        {"platform": "linux-64", "python": [], "version": "1"}]}]}
    problems: List[str] = []
    compare_shape(old_t, new_t, "t", problems)
    assert problems == [], problems
    compare_shape(old_t, {"packages": [{"version": 2, "builds": [{"python": []}]}]}, "t", problems)
    assert len(problems) == 2, problems
    # known_breakages.json: the bundled table is clean, and broken rows are caught.
    bundled = json.loads((DATA_DIR / "known_breakages.json").read_text(encoding="utf-8"))
    assert breakage_problems(bundled) == [], breakage_problems(bundled)
    broken: Dict[str, Any] = {k: [] for k in BREAKAGE_SCHEMA}
    broken["api_removals"] = [{"id": "x", "kind": "magic", "package": "p", "fixed_in": "one",
                               "dependency": "d", "breaks_from": 2, "constraint": "d<2",
                               "symptom": "s", "source": "u", "python_min": "3"}]
    broken["stdlib_removals"] = [{"module": "imp", "removed_in": "3.12", "source": "u"},
                                 {"module": "imp", "removed_in": "3.12", "source": "u"}]
    broken["symbol_removals"] = [{"id": "s", "module": "m", "symbol": "x", "replacement": "y",
                                  "source": "u", "removed_in": "3.10", "dependency": "d",
                                  "breaks_from": "2", "constraint": "d<2"}]
    got = breakage_problems(broken)
    # kind, fixed_in format, breaks_from type, python_min format, duplicate,
    # a symbol row that is both a Python and a distribution removal.
    assert len(got) == 6, got
    # Method rows: a good one passes; a bare call, a bad keyword, a missing
    # symptom and an undotted symbol do not.
    good = {"id": "m", "module": "sqlalchemy", "symbol": "Engine.execute",
            "dependency": "sqlalchemy", "breaks_from": "2", "constraint": "sqlalchemy<2",
            "symptom": "s", "replacement": "r", "source": "u",
            "calls": ["engine.execute(", ".engine.execute(", "conn.execute(\"",
                      "*.append(ignore_index="], "calls_in": ["flask_sqlalchemy"]}
    assert call_problems("r", good) == [], call_problems("r", good)
    bad = dict(good, calls=["execute(", "engine.execute(x"], symbol="execute", calls_in=["a.b"])
    del bad["symptom"]
    assert len(call_problems("r", bad)) == 5, call_problems("r", bad)
    ranged = {k: [] for k in BREAKAGE_SCHEMA}
    ranged["python_removals"] = [
        {"id": "a", "package": "p", "python_min": "3.13", "symptom": "s", "source": "u",
         "breaks_from": "2.0.0", "fixed_in": "2.0.31"},
        {"id": "b", "package": "p", "python_min": "3.13", "symptom": "s", "source": "u",
         "breaks_from": "2.0.31", "fixed_in": "2.0.31"},
        {"id": "c", "package": "p", "python_min": "3.13", "symptom": "s", "source": "u",
         "breaks_from": "2.0.0"}]
    assert len(breakage_problems(ranged)) == 2, breakage_problems(ranged)
    assert "numpy" in breakage_names(bundled) and "2.2.2" in breakage_names(bundled)["pandas"]
    assert "pydantic-settings" in breakage_names(bundled)
    print("self-test passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--self-test", action="store_true",
                        help="run the offline checks of the helpers and exit")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
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

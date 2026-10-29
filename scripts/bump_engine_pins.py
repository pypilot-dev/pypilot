#!/usr/bin/env python3
"""Point the VS Code and pip wrappers at a new engine release.

Reads a release's SHA256SUMS and rewrites the engine pin in each wrapper
checkout given on the command line. Used by .github/workflows/ecosystem-bump.yml
after the Release workflow; runnable by hand too.

Pin contracts (keep in sync with the wrappers and pypilot/AGENTS.md):

  pypilot-vscode/package.json
      "pypilotEngine": { "version": "X.Y.Z",
                         "sha256": { "<asset>": "<hex>", ... } }

  pypilot-pip/src/pypilot/binary.py
      DEFAULT_TAG = "vX.Y.Z"
      ENGINE_SHA256 = {
          "<asset>": "<hex>",
          ...
      }

SHA256SUMS format: one "<64 lowercase hex>  <asset-name>" line per archive,
sorted by name, LF.

Usage:
  bump_engine_pins.py --tag v1.3.0 --sums SHA256SUMS \
      [--vscode path/to/package.json] [--pip path/to/binary.py] [--dry-run]
  bump_engine_pins.py --self-test

Exit codes: 0 ok (files changed, or already current), 1 bad input.
Standard library only; Python 3.8+.
"""

from __future__ import annotations

import argparse
import difflib
import json
import re
import sys
import tempfile
from pathlib import Path
from typing import Callable, Dict, List, Optional, Tuple

# Every archive the Release workflow publishes. SHA256SUMS must list exactly
# these; anything missing means a broken release and nothing is bumped.
ASSETS = (
    "pypilot-darwin-arm64.tar.gz",
    "pypilot-darwin-x64.tar.gz",
    "pypilot-linux-x64.tar.gz",
    "pypilot-windows-x64.zip",
)

TAG_RE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
SUMS_LINE_RE = re.compile(r"^([0-9a-f]{64})  (\S+)$")


class PinError(Exception):
    pass


def parse_tag(tag: str) -> str:
    """Return the bare version ("1.3.0") for a tag ("v1.3.0")."""
    if not TAG_RE.match(tag):
        raise PinError(f"tag must look like vX.Y.Z, got {tag!r}")
    return tag[1:]


def parse_sums(text: str) -> Dict[str, str]:
    """Parse SHA256SUMS into {asset: hex}, enforcing the contract."""
    if "\r" in text:
        raise PinError("SHA256SUMS must use LF line endings")
    sums: Dict[str, str] = {}
    for n, line in enumerate(text.splitlines(), 1):
        if not line:
            continue
        m = SUMS_LINE_RE.match(line)
        if not m:
            raise PinError(f"SHA256SUMS line {n} is not '<sha256>  <asset>': {line!r}")
        digest, name = m.groups()
        if name in sums:
            raise PinError(f"SHA256SUMS lists {name} twice")
        sums[name] = digest
    names = list(sums)
    if names != sorted(names):
        raise PinError("SHA256SUMS is not sorted by asset name")
    missing = [a for a in ASSETS if a not in sums]
    if missing:
        raise PinError(f"SHA256SUMS is missing {', '.join(missing)}")
    return {a: sums[a] for a in ASSETS}


def bump_vscode(text: str, version: str, sums: Dict[str, str]) -> str:
    """Set package.json "pypilotEngine" (appended as the last key if absent).

    A targeted text edit, not a json.dumps round trip, so the rest of the
    file keeps its exact formatting (inline arrays and all).
    """
    data = json.loads(text)
    if not isinstance(data, dict):
        raise PinError("package.json is not a JSON object")
    unit = _json_indent(text)
    value = {"version": version, "sha256": dict(sums)}
    rendered = json.dumps(value, indent=unit, ensure_ascii=False).replace("\n", "\n" + unit)
    span = _top_level_value_span(text, "pypilotEngine")
    if span:
        out = text[: span[0]] + rendered + text[span[1] :]
    else:
        close = text.rstrip().rindex("}")
        last = len(text[:close].rstrip())
        sep = "," if text[last - 1] != "{" else ""
        out = text[:last] + f'{sep}\n{unit}"pypilotEngine": {rendered}' + text[last:]
    if json.loads(out) != dict(data, pypilotEngine=value):
        raise PinError("internal error: edited package.json does not round-trip")
    return out


def _json_indent(text: str) -> str:
    m = re.search(r"\n([ \t]+)\"", text)
    return m.group(1) if m else "  "


_WS = re.compile(r"\s*")
_COLON = re.compile(r"\s*:\s*")
_COMMA = re.compile(r"\s*,?")


def _top_level_value_span(text: str, key: str) -> Optional[Tuple[int, int]]:
    """(start, end) of the value of a top-level key in a JSON object text."""
    dec = json.JSONDecoder()
    i = _WS.match(text, text.index("{") + 1).end()
    while text[i] != "}":
        name, i = dec.raw_decode(text, i)
        i = _COLON.match(text, i).end()
        start = i
        _, i = dec.raw_decode(text, i)
        if name == key:
            return start, i
        i = _WS.match(text, _COMMA.match(text, i).end()).end()
    return None


DEFAULT_TAG_RE = re.compile(r'^DEFAULT_TAG(\s*:\s*str)?\s*=\s*"v[^"]*"', re.M)
# The dict literal: from `ENGINE_SHA256 ... = {` to the first `}` that
# starts a line. Any annotation the wrapper wrote is kept.
ENGINE_SHA_RE = re.compile(r"^(ENGINE_SHA256\b[^=\n]*=\s*)\{.*?^\}", re.M | re.S)


def bump_pip(text: str, tag: str, sums: Dict[str, str]) -> str:
    """Set DEFAULT_TAG and ENGINE_SHA256 in binary.py."""
    if not DEFAULT_TAG_RE.search(text):
        raise PinError('binary.py has no `DEFAULT_TAG = "v..."` line')
    text = DEFAULT_TAG_RE.sub(lambda m: f'DEFAULT_TAG{m.group(1) or ""} = "{tag}"', text, count=1)
    body = "{\n" + "".join(f'    "{a}": "{h}",\n' for a, h in sums.items()) + "}"
    if ENGINE_SHA_RE.search(text):
        return ENGINE_SHA_RE.sub(lambda m: m.group(1) + body, text, count=1)
    # Not there yet (wrapper predates the pin): add it right below DEFAULT_TAG.
    print("note: binary.py had no ENGINE_SHA256; adding it below DEFAULT_TAG", file=sys.stderr)
    line_end = text.index("\n", DEFAULT_TAG_RE.search(text).end())
    block = (
        "\n# SHA-256 of each engine archive for DEFAULT_TAG, from the release's\n"
        "# SHA256SUMS. Written by pypilot/scripts/bump_engine_pins.py.\n"
        f"ENGINE_SHA256 = {body}"
    )
    return text[: line_end] + block + text[line_end:]


def apply(path: Path, edit: Callable[[str], str], dry_run: bool) -> bool:
    """Run `edit` on the file's text with LF endings; write back in the
    file's own line-ending style (a CRLF checkout stays CRLF)."""
    old = path.read_bytes().decode("utf-8")
    crlf = "\r\n" in old
    new = edit(old.replace("\r\n", "\n"))
    if crlf:
        new = new.replace("\n", "\r\n")
    if old == new:
        print(f"{path}: already pinned")
        return False
    diff = difflib.unified_diff(
        old.splitlines(True), new.splitlines(True), str(path), str(path) + " (new)"
    )
    sys.stdout.writelines(diff)
    if not dry_run:
        path.write_bytes(new.encode("utf-8"))  # bytes: no newline translation
        print(f"{path}: updated")
    return True


def run(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--tag", help="engine tag, e.g. v1.3.0")
    ap.add_argument("--sums", type=Path, help="path to the release's SHA256SUMS")
    ap.add_argument("--vscode", type=Path, help="pypilot-vscode package.json")
    ap.add_argument("--pip", type=Path, help="pypilot-pip src/pypilot/binary.py")
    ap.add_argument("--dry-run", action="store_true", help="print the diff, write nothing")
    ap.add_argument("--self-test", action="store_true", help="run the built-in tests")
    args = ap.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.tag or not args.sums or not (args.vscode or args.pip):
        ap.error("--tag, --sums and at least one of --vscode/--pip are required")
    try:
        version = parse_tag(args.tag)
        sums = parse_sums(args.sums.read_bytes().decode("utf-8"))
        if args.vscode:
            apply(args.vscode, lambda t: bump_vscode(t, version, sums), args.dry_run)
        if args.pip:
            apply(args.pip, lambda t: bump_pip(t, args.tag, sums), args.dry_run)
    except (PinError, OSError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


# --------------------------------------------------------------------------
# Self-test: python scripts/bump_engine_pins.py --self-test


def self_test() -> int:
    hexes = {a: (c * 32)[:64] for c, a in zip(("ab", "cd", "ef", "09"), ASSETS)}
    sums_text = "".join(f"{hexes[a]}  {a}\n" for a in sorted(ASSETS))
    failures: List[str] = []

    def check(name: str, cond: bool) -> None:
        print(("ok   " if cond else "FAIL ") + name)
        if not cond:
            failures.append(name)

    def raises(fn, *a) -> bool:
        try:
            fn(*a)
        except PinError:
            return True
        return False

    # SHA256SUMS parsing.
    sums = parse_sums(sums_text)
    check("parses SHA256SUMS", sums == hexes and list(sums) == list(ASSETS))
    check("extra assets are ignored", parse_sums(sums_text + "0" * 64 + "  zz-extra\n") == hexes)
    check("rejects CRLF", raises(parse_sums, sums_text.replace("\n", "\r\n")))
    check("rejects one space", raises(parse_sums, sums_text.replace("  ", " ", 1)))
    check("rejects binary-mode '*'", raises(parse_sums, sums_text.replace("  ", " *", 1)))
    check("rejects uppercase hex", raises(parse_sums, sums_text[:64].upper() + sums_text[64:]))
    check("rejects missing asset", raises(parse_sums, "".join(sums_text.splitlines(True)[1:])))
    check("rejects unsorted", raises(parse_sums, "".join(reversed(sums_text.splitlines(True)))))
    check("tag must be vX.Y.Z", raises(parse_tag, "1.2.3") and parse_tag("v1.2.3") == "1.2.3")

    # VS Code package.json.
    pkg = '{\n  "name": "pypilot-vscode",\n  "version": "1.2.1",\n  "engines": {\n    "vscode": "^1.85.0"\n  }\n}\n'
    out = bump_vscode(pkg, "1.3.0", sums)
    data = json.loads(out)
    check("vscode: adds pypilotEngine", data["pypilotEngine"] == {"version": "1.3.0", "sha256": hexes})
    check("vscode: keeps other fields and order", list(data)[:3] == ["name", "version", "engines"] and data["version"] == "1.2.1")
    check("vscode: keeps 2-space indent and final newline", out.startswith('{\n  "name"') and out.endswith("}\n"))
    check("vscode: idempotent", bump_vscode(out, "1.3.0", sums) == out)
    old = bump_vscode(pkg, "1.2.2", {a: "0" * 64 for a in ASSETS})
    check("vscode: replaces an older pin", json.loads(bump_vscode(old, "1.3.0", sums))["pypilotEngine"]["version"] == "1.3.0")
    inline = '{\n  "a": ["x", "y"],\n  "nested": { "pypilotEngine": 1 },\n  "z": { "k": [1, 2] }\n}\n'
    out = bump_vscode(inline, "1.3.0", sums)
    check("vscode: other formatting untouched", out.startswith(inline[: inline.rindex("\n}")]) and out.endswith("    }\n  }\n}\n"))
    check("vscode: nested same-named key ignored", json.loads(out)["nested"] == {"pypilotEngine": 1})
    moved = pkg.replace('{\n  "name"', '{\n  "pypilotEngine": {"version": "0.0.1", "sha256": {}},\n  "name"', 1)
    out = bump_vscode(moved, "1.3.0", sums)
    check("vscode: replaces a pin in place", list(json.loads(out))[0] == "pypilotEngine" and json.loads(out)["pypilotEngine"]["sha256"] == hexes and bump_vscode(out, "1.3.0", sums) == out)
    tabs = '{\n\t"name": "x"\n}'
    out = bump_vscode(tabs, "1.3.0", sums)
    check("vscode: tab indent, no final newline", out.startswith('{\n\t"name": "x",\n\t"pypilotEngine": {\n\t\t"version"') and not out.endswith("\n"))

    # pip binary.py.
    py = (
        'GITHUB_REPO = "pypilot-dev/pypilot"\n'
        '# The engine release this package downloads by default.\n'
        'DEFAULT_TAG = "v1.2.2"\n'
        "MIN_VERSION = (1, 0, 2)\n"
        "ENGINE_SHA256: Dict[str, str] = {\n"
        '    "pypilot-linux-x64.tar.gz": "' + "0" * 64 + '",\n'
        "}\n"
        "\n"
        "def f():\n"
        "    return {}\n"
    )
    out = bump_pip(py, "v1.3.0", sums)
    check("pip: bumps DEFAULT_TAG", 'DEFAULT_TAG = "v1.3.0"\n' in out and "v1.2.2" not in out)
    check("pip: rewrites ENGINE_SHA256, keeps annotation", out.count("ENGINE_SHA256: Dict[str, str] = {\n") == 1 and all(f'    "{a}": "{h}",\n' in out for a, h in hexes.items()) and "0" * 64 not in out)
    check("pip: leaves the rest alone", out.endswith("\n\ndef f():\n    return {}\n") and "MIN_VERSION = (1, 0, 2)\n" in out)
    check("pip: idempotent", bump_pip(out, "v1.3.0", sums) == out)
    bare = py.replace(py[py.index("ENGINE_SHA256"): py.index("}\n") + 2], "")
    added = bump_pip(bare, "v1.3.0", sums)
    check("pip: adds ENGINE_SHA256 when absent", "ENGINE_SHA256 = {\n" in added and bump_pip(added, "v1.3.0", sums) == added)
    ns: Dict[str, object] = {}
    exec(compile(added, "binary.py", "exec"), ns)  # noqa: S102 - our own fixture
    check("pip: result is valid Python", ns["ENGINE_SHA256"] == hexes and ns["DEFAULT_TAG"] == "v1.3.0")
    check("pip: needs DEFAULT_TAG", raises(bump_pip, "X = 1\n", "v1.3.0", sums))

    # End to end through the CLI, dry run then real.
    with tempfile.TemporaryDirectory() as d:
        dp = Path(d)
        (dp / "SHA256SUMS").write_bytes(sums_text.encode())
        (dp / "package.json").write_bytes(pkg.encode())
        (dp / "binary.py").write_bytes(py.encode())
        argv = ["--tag", "v1.3.0", "--sums", str(dp / "SHA256SUMS"), "--vscode", str(dp / "package.json"), "--pip", str(dp / "binary.py")]
        rc = run(argv + ["--dry-run"])
        check("cli: dry run writes nothing", rc == 0 and (dp / "package.json").read_bytes() == pkg.encode() and (dp / "binary.py").read_bytes() == py.encode())
        rc = run(argv)
        written = (dp / "binary.py").read_bytes()
        check("cli: writes LF files", rc == 0 and b"\r" not in written and b"v1.3.0" in written)
        crlf_pkg = pkg.replace("\n", "\r\n").encode()
        (dp / "package.json").write_bytes(crlf_pkg)
        run(argv)
        after = (dp / "package.json").read_bytes()
        check("cli: CRLF checkout stays CRLF", after.startswith(crlf_pkg[:-5]) and b"\n" not in after.replace(b"\r\n", b""))
        (dp / "SHA256SUMS").write_bytes(b"garbage\n")
        check("cli: bad SHA256SUMS exits 1", run(argv) == 1)

    print(f"\n{len(failures)} failure(s)" if failures else "\nall self-tests passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(run())

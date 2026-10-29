# Troubleshooting & FAQ

## Filing an issue

The fastest route to a fix is a report that shows what PyPilot saw. Please include:

1. **The engine version**: `pypilot --version`, or with an engine newer than 1.2.2, `pypilot version` (it adds the commit and the data-table state).
2. **Your OS and editor** (Zed, VS Code, or the CLI alone).
3. **The doctor report** from the project folder. With an engine newer than 1.2.2, attach the JSON: it holds the whole assessment (environment, interpreters, every dependency's range, findings, data tables) in one block, and nothing has to be retyped:

   ```bash
   pypilot doctor --json > pypilot-doctor.json
   pypilot doctor --markdown        # the same, readable, to paste into the issue text
   pypilot explain --json           # what blocks each Python
   ```

   With engine 1.2.2, paste the output of `pypilot doctor`. See [Machine-readable output](/engine/output).
4. **What PyPilot ran**, if a command failed (engine newer than 1.2.2): `pypilot logs --tail 3`. It lists each subprocess with its exit code and the end of its stderr. Credentials are masked, but paths and package names are in it; read it before posting. See [Logs](/engine/logs).
5. **For GPU problems**: the GPU model and driver version from `nvidia-smi`.
6. **A minimal `requirements.txt` or `pyproject.toml`** that reproduces it.

Where to file: what PyPilot *decides* (Python version, CUDA build, flagged imports) goes to [pypilot](https://github.com/pypilot-dev/pypilot/issues/new?template=bug_report.md); how it shows up in VS Code to [pypilot-vscode](https://github.com/pypilot-dev/pypilot-vscode/issues); installing through pip to [pypilot-pip](https://github.com/pypilot-dev/pypilot-pip/issues). Not sure? Use the engine repo; it will be moved. Security problems: please do not open a public issue; see [Security](./security#reporting-a-vulnerability).

## Common situations

### "No environment set up yet" but I have one

PyPilot looks for `.venv` in the project root (or the pixi environment in pixi projects). An environment elsewhere, or a conda environment, is not the project's environment as far as PyPilot is concerned. Run `pypilot setup` to create `.venv`, or migrate a conda project ([Conda projects](./conda)).

### It recommends a Python I don't have

In uv mode PyPilot installs it with `uv python install`. In pip mode it never downloads interpreters; install the version it names (python.org, your package manager, or `pyenv`) and run `pypilot setup` again. Or install uv and switch to `auto`.

### Dependencies conflict

```text
  [ERROR] Dependencies conflict on Python version
          tensorflow supports up to Python 3.10 but numpy requires at least Python 3.12 — no single interpreter satisfies both.
```

There is no single Python to fix this with, so there is no auto-fix button. Run `pypilot explain` to see what blocks each version, then relax or change one pin (here `numpy<2.3`, or a newer tensorflow).

### "Could not resolve `name`"

The name is not on PyPI: a typo, or a private package. PyPilot cannot judge packages it cannot see; the rest of the assessment still runs.

### "Will compile from source"

A dependency has no wheel for your platform or Python, so pip/uv will build its sdist. That needs a compiler, and sometimes system libraries. If conda-forge has a build, PyPilot suggests [pixi](/pixi/advisory).

### Zed's Python server still says packages are missing

Zed's own Python language server may be pointed at another interpreter. PyPilot offers to pin pyright to the project environment; accept it, or set the interpreter in Zed's toolchain selector. See [Zed](/zed/#what-you-see).

### VS Code keeps the old engine after I updated it

The running language server holds the old binary. Run **Developer: Reload Window**.

### The editor stopped responding {#the-editor-stopped-responding}

No squiggles any more, buttons that do nothing, or a message that the server crashed or "will not be restarted": the `pypilot lsp` process was killed (by you, an antivirus, a replaced binary, or a crash), and the editor gave up restarting it.

- **VS Code**: run **Developer: Reload Window**. The **PyPilot** output channel shows what happened.
- **Zed**: run `editor: restart language server`, or reopen the folder.

If it keeps happening, `pypilot logs --tail` (engine newer than 1.2.2) shows the last actions the engine recorded; please attach it to an issue.

### Working offline, or behind a proxy {#offline}

PyPilot keeps working from its caches and the tables compiled into it; only packages it has never looked up cannot be judged.

- `pypilot doctor` lists each package it could not look up as **not checked (PyPI unreachable)** and adds one note, *Could not reach PyPI for some dependencies*, with the reason. Those packages do not narrow the supported range, so re-run it online before trusting a recommendation. <Badge type="tip" text="next" /> Engine 1.2.2 reported each of them as *Could not resolve `name` … It may be a typo*.
- In the editor, <Badge type="tip" text="next" /> those imports get no diagnostic. "PyPI has no project named …" only appears when PyPI really answered 404. Engine 1.2.2 showed it for every import it could not look up, offline included.
- `pypilot update-data` prints `failed` per table and keeps the copies it has; <Badge type="tip" text="next" /> `update-data --check` exits `2`.
- The engine reads `HTTPS_PROXY` / `NO_PROXY` for its own requests. Downloads of the engine itself: see [VS Code](/vscode/#a-pinned-verified-engine) and [pypilot-cli](/python/#offline).

To stay offline on purpose, see [offline use](./security#offline-use).

### A data table could not be refreshed

`pypilot update-data` prints `failed <table>` and keeps the snapshot built into the engine; nothing breaks. Check your network, or set `data_refresh_days = 0` to stop trying.

### I don't want notifications

Set `notifications = "off"` (VS Code: `pypilot.notifications`). Note that `off` also turns off import diagnostics. **Never for this project** silences one project and is stored in `.zed/pypilot.toml`.

### Windows: Zed task fails with "os error 193"

See the tip on the [Zed page](/zed/#commands-as-tasks).

## FAQ

**Does PyPilot send my code anywhere?** No. It queries PyPI for package metadata, GitHub for engine releases and data tables, and (for the pixi migration) prefix.dev's public name mapping. No telemetry, no AI service, no account.

**Does it work offline?** Yes, with cached metadata. Set `data_refresh_days = 0` and keep a local engine; see [offline use](./security#offline-use) and [what you see offline](#offline).

**Does it log anything?** <Badge type="tip" text="next" /> A local log of the commands it ran, in the cache directory; nothing is sent. `PYPILOT_NO_LOG=1` turns it off. See [Logs](/engine/logs).

**Does it replace uv, pip or pixi?** No, it drives them.

**Which Python versions does it know about?** CPython 3.7 to 3.15.

**Does it support PyPy, or Python 2?** It reasons about CPython 3.7 and later.

**Why does it pick the newest Python in range?** The newest supported interpreter gets the longest support window, and the range already guarantees every dependency has a wheel for it. Your `requires-python` or a pixi `python` spec caps it.

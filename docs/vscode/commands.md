# Commands & tasks

## Command palette

| Command | ID | Does |
|---|---|---|
| PyPilot: Set Up Environment | `pypilot.setup` | Builds the environment, installing Python if needed (uv mode), and installs the dependencies. Same as `pypilot setup`. |
| PyPilot: Doctor: Show Environment Report | `pypilot.doctor` | Runs the health check and opens the report. |
| PyPilot: Fix Python Version | `pypilot.fixPython` | Rebuilds the environment on a supported Python. Same as `pypilot fix python`. |
| PyPilot: Fix CUDA / PyTorch Build | `pypilot.fixCuda` | Re-pins torch/tensorflow to the build for your driver. Same as `pypilot fix cuda`. |
| PyPilot: Check Package Compatibility… | `pypilot.check` | Asks for a package (or requirement) and adds a `pypilot check` section to the report. |
| PyPilot: Install Package… | `pypilot.install` | Asks for a package, installs it and records it in the manifest. |
| PyPilot: Update Bundled Data Tables | `pypilot.updateData` | Refreshes the CUDA, framework and import tables now. |
| PyPilot: Migrate Conda environment.yml to pyproject.toml | `pypilot.migrateConda` | Same as `pypilot migrate-conda`. |
| PyPilot: Open Environment Report Panel | `pypilot.openReport` | Runs the health check and opens the report (in the webview panel <Badge type="tip" text="next" />). |
| PyPilot: Install uv Globally | `pypilot.installUv` | Installs uv so it is available outside this workspace too. |
| PyPilot: Check for Helper Updates | `pypilot.updateHelper` | Compares the engine against the latest GitHub release (next release: offers the pinned version when it is newer than the engine in use; with `pypilot.engineUpdates: "latest"` also newer releases). |
| PyPilot: Use Project Environment (.venv / pixi) | `pypilot.useProjectVenv` | Makes the project environment the active interpreter. |
| PyPilot: Install pixi <Badge type="tip" text="next" /> | `pypilot.installPixiCli` | Installs pixi with its official installer, through the engine; with an older engine it offers to run the installer in a terminal. |
| PyPilot: Initialize pixi project <Badge type="tip" text="next" /> | `pypilot.initPixiProject` | Shows the `pixi.toml` PyPilot would write, then writes it (optionally running `pixi install`) only after you confirm. |

Every command that changes the environment shows a progress notification and updates the status bar from the result; a failed setup is reported as a failure.

### Hidden where they do not apply <Badge type="tip" text="next" />

Project-specific commands are hidden from the palette where they cannot apply. They still run from the walkthrough, keybindings and the report panel.

| Command | Shown only when |
|---|---|
| Fix CUDA / PyTorch Build | a manifest (`pyproject.toml`, `requirements*.txt`, `pixi.toml`, `environment.yml`) names `torch` or `tensorflow` |
| Migrate Conda environment.yml to pyproject.toml | an `environment.yml` (or `.yaml`) exists |
| Install pixi | pixi is not on `PATH` or in `~/.pixi/bin` |
| Initialize pixi project | a folder is not a pixi project yet |

## Tasks

**Tasks: Run Task → pypilot** lists tasks that run the CLI in the integrated terminal, so you see streaming output. You can also declare them in `tasks.json`:

```json
{
  "version": "2.0.0",
  "tasks": [
    { "type": "pypilot", "task": "setup", "label": "pypilot: setup" },
    { "type": "pypilot", "task": "doctor", "label": "pypilot: doctor" }
  ]
}
```

| `task` | Runs |
|---|---|
| `setup` | `pypilot setup` |
| `doctor` | `pypilot doctor` |
| `fix python` | `pypilot fix python` |
| `fix cuda` | `pypilot fix cuda` |
| `update-data` | `pypilot update-data` |
| `migrate-conda` | `pypilot migrate-conda` |

## The notification

On open, one notification at most. **Fix environment** runs the same code as `pypilot setup`; **Show details** opens the report; **Ignore** dismisses it; **Never for this project** is remembered in `.zed/pypilot.toml` as `[state] never = true`. With engine support for the pixi advisory, an **Initialize pixi project** button appears when pixi would help. How often you are notified is set by [`pypilot.notifications`](./settings).

When uv is missing in `auto` mode, **Fix environment** first offers to install it (about 18 MB, once, globally) and asks whether to use a uv project (`pyproject.toml` + lockfile) or the pip-compatible mode (`requirements.txt`, faster through uv). **No thanks** continues with pip.

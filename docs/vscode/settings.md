# VS Code settings

Every setting can be set per workspace folder (scope `resource`) except two: `pypilot.helperPath` is a **machine** setting (user settings only, so a cloned repository cannot point VS Code at a binary of its choosing) and `pypilot.engineUpdates` is an **application** setting.

| Setting | Default | Values | Description |
|---|---|---|---|
| `pypilot.packageManager` | `"auto"` | `auto`, `uv`, `uv-pip`, `pip`, `pixi` <Badge type="tip" text="next" /> | How environments are built and packages installed. `auto` uses uv when it is installed (and offers to install it), else pip. pixi projects always use pixi. Engine key: [`package_manager`](/reference/configuration#package-manager). |
| `pypilot.notifications` | `"problems-only"` | `all`, `problems-only`, `off` | When to show notifications. `off` also turns off import diagnostics. |
| `pypilot.autoCheckOnOpen` | `true` | boolean | Scan the workspace when it opens. |
| `pypilot.autoSelectVenv` | `true` | boolean | Make the project environment the active interpreter, and so the recommended notebook kernel, when it is created or changes, and on startup when no interpreter inside the workspace is selected. Extension-only. |
| `pypilot.dataRefreshDays` | `7` | integer | How often to refresh the bundled data tables. `0` means fully offline. |
| `pypilot.pixiEnvironment` <Badge type="tip" text="next" /> | `"default"` | a pixi environment name | Which pixi environment PyPilot and interpreter selection use in multi-environment pixi projects (the `[environments]` table of `pixi.toml`), for example `cuda`. Install it with `pixi install -e <name>`. Ignored outside pixi projects. |
| `pypilot.helperPath` <Badge type="tip" text="next" /> | `""` | absolute path | A `pypilot` executable to use instead of the downloaded engine, for example a local `target/release/pypilot`. It must answer `--version`; while set, nothing is downloaded and no engine updates are offered. Machine setting: a workspace cannot set it. |
| `pypilot.engineUpdates` <Badge type="tip" text="next" /> | `"pinned"` | `pinned`, `latest` | `pinned` offers only the engine version this extension release pins. `latest` also offers newer GitHub releases, verified against their `SHA256SUMS` and refused without one. |
| `pypilot.diagnosticsScope` <Badge type="tip" text="next" /> | `"openFiles"` | `openFiles`, `workspace` | `openFiles` checks imports in open files only. `workspace` also scans every Python file in the background, so the Problems panel lists files you have not opened. Needs an engine that supports it; older engines keep checking open files only. |

### pypilot.pixiEnvironment

Names are lowercase letters, digits and dashes; anything else is ignored with a log line. Changing it selects the new environment's interpreter and re-checks the open buffers against its packages.

### pypilot.diagnosticsScope

The engine key is [`diagnostics_scope`](/reference/configuration#diagnostics-scope); what the background scan skips and its limits are listed there. A `pypilot.toml` in the project that sets `diagnostics_scope` wins over this setting.

## How settings reach the engine <Badge type="tip" text="next" />

The extension sends its settings to the engine: `initializationOptions` at startup (`pixiEnvironment`, `diagnosticsScope`) and the whole `pypilot` section through `workspace/didChangeConfiguration`, once after startup and on every change. In multi-root windows both are read for each server's folder. The engine layers them between your global config file and the project's `pypilot.toml` (see [precedence](/reference/configuration#precedence)); a value equal to the built-in default counts as *unset*, so it never hides a value from your global config file.

With VS Code 1.2.1 and engine 1.2.2, the engine reads only the TOML files, so put settings you need applied in `pypilot.toml` (or `.zed/pypilot.toml`), or the global config file.

## Example

```json
{
  "pypilot.packageManager": "uv",
  "pypilot.notifications": "all",
  "pypilot.pixiEnvironment": "cuda"
}
```


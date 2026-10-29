//! The LSP server: workspace onboarding (F5) and the live import guardian (F4).
//!
//! Two surfaces share one engine.
//!
//! On workspace open, a probe and compatibility pass raises at most one
//! notification with `[Fix environment] [Show details] [Ignore] [Never for this
//! project]`. If the environment is already correct it stays quiet.
//!
//! On Python buffers, imports that cannot resolve become diagnostics with quick
//! fixes. Analysis runs on open and save immediately, and one second after the
//! user stops typing. Each edit supersedes the pending run for that buffer, so a
//! burst of keystrokes costs one analysis rather than one per character.
//!
//! VS Code sends each Jupyter notebook cell as its own document. Cells are
//! analyzed like files, with IPython magics masked out, and published per cell.
//!
//! A CLI run started from a Zed task leaves a request file behind (see
//! [`crate::core::rescan`]); a watcher polls it and re-raises the notification,
//! which is how a terminal command surfaces in the editor.

mod diagnostics;
mod pixi;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::core::platform::Platform;
use crate::core::{
    env, guardian, imports, install, project, pyright, rescan, setup, solver, Assessment, FixKind,
    Severity,
};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::PyPiClient;
use crate::settings::{Notifications, PackageManager, ResolvedMode, Settings};

use crate::report::markdown::{details_markdown, package_check_markdown};
use diagnostics::{Scope, CMD_INSTALL, CMD_RECREATE};

// F5 commands.
const CMD_FIX: &str = "pypilot.fixEverything";
const CMD_DETAILS: &str = "pypilot.showDetails";
const CMD_IGNORE: &str = "pypilot.ignore";
const CMD_NEVER: &str = "pypilot.neverForProject";
const CMD_INSTALL_UV: &str = "pypilot.installUvGlobally";
// pixi's commands (installPixi, migrateToPixi) live in `pixi.rs`.

// Notification button labels.
const BTN_FIX: &str = "Fix environment";
const BTN_DETAILS: &str = "Show details";
const BTN_IGNORE: &str = "Ignore";
const BTN_NEVER: &str = "Never for this project";
const BTN_PIXI: &str = "Initialize pixi project";

const BTN_INSTALL_UV: &str = "Install uv";
const BTN_NO_THANKS: &str = "No thanks";
const BTN_MODE_UV_PROJECT: &str = "uv project (pyproject.toml + lockfile)";
const BTN_MODE_PIP_COMPAT: &str = "pip-compatible (requirements.txt, faster via uv)";

const BTN_PIN_PYRIGHT: &str = "Point pyright at the environment";
const BTN_NOT_NOW: &str = "Not now";

/// Whether the client is Zed. Only Zed's own Python servers are pointed at
/// the wrong environment by its toolchain selection, so only Zed users are
/// offered the pyright pin; VS Code has its own interpreter picker.
static CLIENT_IS_ZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long the buffer must be idle before the guardian runs.
const IDLE_DEBOUNCE: Duration = Duration::from_secs(1);

/// VS Code syncs each notebook cell as its own text document under this scheme,
/// e.g. `vscode-notebook-cell:/c%3A/proj/analysis.ipynb#W2sZmlsZQ%3D%3D`: the
/// path is the notebook, the fragment names the cell.
const NOTEBOOK_CELL_SCHEME: &str = "vscode-notebook-cell";

/// An open buffer, as of the client's latest version of it.
struct Document {
    version: i32,
    text: String,
    /// Only consulted for notebook cells, where markdown cells share the scheme.
    language_id: String,
}

/// State shared between request handlers and spawned background tasks.
struct Shared {
    client: Client,
    /// The project PyPilot sets up: the worktree itself, or its one nested
    /// Python project (see [`project::project_root`]).
    root: Mutex<Option<PathBuf>>,
    /// Open buffers, by URI.
    documents: Mutex<HashMap<Url, Document>>,
    /// Edit counter per buffer. A debounced run compares against this and exits
    /// if the buffer moved on, which is the whole cancellation mechanism.
    revisions: Mutex<HashMap<Url, u64>>,
    /// Diagnostics last published per URI. A code action request only carries
    /// what is selected, and a notebook selection never spans cells, so the
    /// whole-file and whole-notebook actions are built from this instead.
    published: Mutex<HashMap<Url, Vec<Diagnostic>>>,
    source: PyPiClient,
}

struct Backend {
    shared: Arc<Shared>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        *self.shared.root.lock().await = workspace_root(&params).map(|w| project::project_root(&w));
        let zed = params
            .client_info
            .as_ref()
            .is_some_and(|c| is_zed_client(&c.name));
        CLIENT_IS_ZED.store(zed, std::sync::atomic::Ordering::Relaxed);
        // Editor settings (`pixiEnvironment`, ...) layer between the global
        // and the project config file; see `crate::settings`.
        if let Some(options) = &params.initialization_options {
            crate::settings::set_editor_settings(options);
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "PyPilot".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        CMD_FIX.into(),
                        CMD_DETAILS.into(),
                        CMD_IGNORE.into(),
                        CMD_NEVER.into(),
                        CMD_INSTALL.into(),
                        CMD_RECREATE.into(),
                        CMD_INSTALL_UV.into(),
                    ]
                    .into_iter()
                    .chain(pixi::COMMANDS.iter().map(|c| c.to_string()))
                    .collect(),
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let Some(root) = self.shared.root.lock().await.clone() else {
            return;
        };
        tokio::spawn(watch_rescan_requests(self.shared.clone(), root.clone()));
        // The onboarding toast waits on the user, possibly forever. tower-lsp
        // handles only a few messages at once, so awaiting it here would hold
        // one of those slots for as long as the toast stays unanswered.
        let shared = self.shared.clone();
        tokio::spawn(async move {
            scan_and_notify(&shared.client, &shared.source, &root, false).await;
        });
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let document = Document {
            version: params.text_document.version,
            text: params.text_document.text,
            language_id: params.text_document.language_id,
        };
        self.shared
            .documents
            .lock()
            .await
            .insert(uri.clone(), document);
        run_guardian(self.shared.clone(), uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        // FULL sync: the last content change is the whole document.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        {
            let mut documents = self.shared.documents.lock().await;
            // tower-lsp runs notifications concurrently, so two quick edits can
            // be handled out of order. Never let the older text win.
            let language_id = match documents.get(&uri) {
                Some(d) if d.version >= version => return,
                Some(d) => d.language_id.clone(),
                None => String::new(),
            };
            documents.insert(
                uri.clone(),
                Document {
                    version,
                    text: change.text,
                    language_id,
                },
            );
        }

        let revision = {
            let mut revisions = self.shared.revisions.lock().await;
            let counter = revisions.entry(uri.clone()).or_insert(0);
            *counter += 1;
            *counter
        };

        let shared = self.shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DEBOUNCE).await;
            // Superseded by a newer edit: that run will handle it.
            if shared.revisions.lock().await.get(&uri).copied() != Some(revision) {
                return;
            }
            run_guardian(shared, uri).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // A save is an explicit checkpoint; do not make the user wait a second.
        run_guardian(self.shared.clone(), params.text_document.uri).await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // VS Code sends `{ pypilot: { pixiEnvironment, packageManager, ... } }`
        // once after startup and on every change. A new pixi environment
        // changes what is installed, so re-check the open buffers.
        crate::settings::set_editor_settings(&params.settings);
        tokio::spawn(refresh_all_buffers(self.shared.clone()));
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.shared.documents.lock().await.remove(&uri);
        self.shared.revisions.lock().await.remove(&uri);
        self.shared.published.lock().await.remove(&uri);
        // Clear our squiggles so they do not outlive the buffer.
        self.shared
            .client
            .publish_diagnostics(uri, vec![], None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let (whole, scope) = {
            let published = self.shared.published.lock().await;
            match notebook_url(uri) {
                // Every open cell of the same notebook, in a stable order.
                Some(notebook) => {
                    let mut cells: Vec<(&Url, &Vec<Diagnostic>)> = published
                        .iter()
                        .filter(|(cell, _)| notebook_url(cell).as_ref() == Some(&notebook))
                        .collect();
                    cells.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                    let all = cells.into_iter().flat_map(|(_, d)| d.clone()).collect();
                    (all, Scope::Notebook)
                }
                None => (published.get(uri).cloned().unwrap_or_default(), Scope::File),
            }
        };
        let actions = diagnostics::code_actions_in(&params.context.diagnostics, &whole, scope);
        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> RpcResult<Option<serde_json::Value>> {
        // One log line per command (`pypilot logs`), steps included.
        let root = self.shared.root.lock().await.clone();
        let args: Vec<String> = params
            .arguments
            .iter()
            .map(|a| a.as_str().map_or_else(|| a.to_string(), str::to_string))
            .collect();
        let action = crate::core::logs::Action::new("lsp", &params.command, &args)
            .workspace(root.as_deref());
        crate::core::logs::scope(action, self.execute_command_logged(params), |r| {
            let ok = match r {
                Ok(Some(v)) => v.get("ok").and_then(|ok| ok.as_bool()) != Some(false),
                Ok(None) => true,
                Err(e) => return (false, Some(e.to_string())),
            };
            (ok, None)
        })
        .await
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }
}

impl Backend {
    async fn execute_command_logged(
        &self,
        params: ExecuteCommandParams,
    ) -> RpcResult<Option<serde_json::Value>> {
        let root = self.shared.root.lock().await.clone();
        let Some(root) = root else { return Ok(None) };
        let client = &self.shared.client;

        // pixi commands answer with their own JSON (see `pixi`).
        if let Some(result) = pixi::execute(&params.command, client, &root, &params.arguments).await
        {
            return Ok(Some(result));
        }

        // Commands that change the environment answer `{"ok": bool}`. Zed
        // ignores the result; the VS Code extension reads it so that a failed
        // setup is not reported as a success.
        let ok = match params.command.as_str() {
            CMD_FIX => Some(run_fix(client, &self.shared.source, &root).await),
            CMD_DETAILS => {
                // An optional package argument adds a `pypilot check` section,
                // which is how the VS Code "check package" command uses this.
                let package = string_arg(&params.arguments, 0);
                show_details(client, &self.shared.source, &root, package.as_deref()).await;
                None
            }
            CMD_NEVER => {
                mark_never(client, &root).await;
                None
            }
            CMD_IGNORE => None,

            // `[package, ...]`: one or more names, installed together.
            CMD_INSTALL => {
                let Some(packages) = string_args(&params.arguments, 0) else {
                    return Ok(None);
                };
                Some(run_install(self.shared.clone(), &root, &packages).await)
            }

            // `[python, package, ...]`: one rebuild, then every package.
            CMD_RECREATE => {
                let (Some(python), Some(packages)) = (
                    string_arg(&params.arguments, 0),
                    string_args(&params.arguments, 1),
                ) else {
                    return Ok(None);
                };
                Some(run_recreate(self.shared.clone(), &root, &python, &packages).await)
            }

            CMD_INSTALL_UV | "pypilot.installUv" => Some(run_install_uv(client).await),

            other => {
                client
                    .log_message(MessageType::WARNING, format!("unknown command `{other}`"))
                    .await;
                None
            }
        };
        Ok(ok.map(|ok| serde_json::json!({ "ok": ok })))
    }
}

// --- F4: the guardian pass ---------------------------------------------------

/// Analyze one buffer and publish its diagnostics.
async fn run_guardian(shared: Arc<Shared>, uri: Url) {
    let cell = uri.scheme() == NOTEBOOK_CELL_SCHEME;
    // The file on disk, used to find modules sitting next to it. An untitled
    // notebook has none, and its cells are still worth checking.
    let path = if cell {
        notebook_url(&uri).and_then(|u| u.to_file_path().ok())
    } else {
        let Ok(path) = uri.to_file_path() else { return };
        if path.extension().is_some_and(|e| e != "py") {
            return;
        }
        Some(path)
    };

    let Some(root) = shared.root.lock().await.clone() else {
        return;
    };
    let Some(text) = shared.documents.lock().await.get(&uri).and_then(|d| {
        // A notebook's markdown cells arrive under the same scheme.
        if cell && d.language_id != "python" {
            None
        } else if cell {
            Some(imports::mask_ipython(&d.text))
        } else {
            Some(d.text.clone())
        }
    }) else {
        return;
    };

    let settings = Settings::load(&root);
    if settings.notifications == Notifications::Off {
        return;
    }

    // Everything here is local except the PyPI lookups, which are cached and
    // only happen for imports that are neither stdlib, local, nor installed.
    // The project's environment: its `.venv`, or its pixi prefix.
    let env = env::project_env(&root);
    // An environment whose Python is gone can't import anything, so
    // per-import verdicts against it would be wrong either way. The
    // workspace scan reports it once, with the rebuild fix; until then, stay
    // quiet here.
    let findings = if env.as_ref().is_some_and(|e| e.broken.is_some()) {
        Vec::new()
    } else {
        let installed = env::importable(&root, env.as_ref());
        let venv_python = env.as_ref().and_then(|e| e.python);
        let venv = env.map(|e| e.path).unwrap_or_else(|| root.join(".venv"));

        let mut findings = guardian::analyze_buffer_in(
            &text,
            &root,
            path.as_deref().and_then(Path::parent),
            Platform::current(),
            venv_python,
            &installed,
            &shared.source,
        )
        .await;

        // Packages that are installed still have to expose what the file uses.
        // Purely local, so this costs a directory listing rather than a request.
        findings.extend(guardian::check_attributes(&text, &venv, &installed));
        findings
    };

    let pixi = crate::core::pixi::detect(&root).is_some();
    let published: Vec<Diagnostic> = findings
        .iter()
        .map(|f| diagnostics::to_diagnostic_in(f, pixi))
        .collect();
    {
        // Closed while the analysis ran: storing would leak the entry and
        // publishing would bring back squiggles did_close just cleared.
        if !shared.documents.lock().await.contains_key(&uri) {
            return;
        }
        shared
            .published
            .lock()
            .await
            .insert(uri.clone(), published.clone());
    }
    shared
        .client
        .publish_diagnostics(uri, published, None)
        .await;
}

/// The notebook a `vscode-notebook-cell` URI belongs to, as a `file` URL: same
/// path, cell fragment dropped. `None` for anything that is not a cell.
///
/// Built from the encoded path rather than via `set_scheme`, which refuses to
/// move a URL from a custom scheme to `file`.
fn notebook_url(uri: &Url) -> Option<Url> {
    // An untitled notebook's path is relative (`Untitled-1.ipynb`), and glued
    // after `file://` it would read as a network host.
    if uri.scheme() != NOTEBOOK_CELL_SCHEME || !uri.path().starts_with('/') {
        return None;
    }
    let host = uri.host_str().unwrap_or("");
    Url::parse(&format!("file://{host}{}", uri.path())).ok()
}

/// Re-analyze every open buffer, so fixed imports stop being marked.
async fn refresh_all_buffers(shared: Arc<Shared>) {
    let uris: Vec<Url> = shared.documents.lock().await.keys().cloned().collect();
    for uri in uris {
        run_guardian(shared.clone(), uri).await;
    }
}

/// Validate the requested names, telling the user which one was refused.
async fn checked_packages(client: &Client, packages: &[String]) -> Option<Vec<String>> {
    match install::validate_package_names(packages) {
        Ok(packages) => Some(packages),
        Err(bad) => {
            client
                .show_message(
                    MessageType::ERROR,
                    format!("PyPilot: `{bad}` is not a valid package name."),
                )
                .await;
            None
        }
    }
}

/// Returns whether every package ended up installed.
async fn run_install(shared: Arc<Shared>, root: &Path, packages: &[String]) -> bool {
    let client = &shared.client;
    let Some(packages) = checked_packages(client, packages).await else {
        return false;
    };
    let package = packages.join(", ");

    let settings = Settings::load(root);
    client
        .show_message(MessageType::INFO, format!("PyPilot: installing {package}…"))
        .await;

    let ok = match install::install_packages(root, &settings, &packages).await {
        Ok(summary) if summary.ok => {
            client
                .show_message(MessageType::INFO, format!("PyPilot: installed {package}."))
                .await;
            true
        }
        Ok(summary) => {
            report_failed_step(client, root, &summary).await;
            false
        }
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: install failed. {e}"))
                .await;
            false
        }
    };
    refresh_all_buffers(shared).await;
    ok
}

/// Returns whether the rebuild and every install finished.
async fn run_recreate(shared: Arc<Shared>, root: &Path, python: &str, packages: &[String]) -> bool {
    let client = &shared.client;
    let Some(version) = PyVersion::parse(python) else {
        client
            .show_message(
                MessageType::ERROR,
                format!("PyPilot: `{python}` is not a Python version."),
            )
            .await;
        return false;
    };
    let Some(packages) = checked_packages(client, packages).await else {
        return false;
    };
    let package = packages.join(", ");

    let settings = Settings::load(root);
    client
        .show_message(
            MessageType::INFO,
            format!("PyPilot: rebuilding the environment on Python {version}…"),
        )
        .await;

    let ok = match install::recreate_with_python(root, &settings, version, &packages).await {
        Ok(summary) if summary.ok => {
            client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "PyPilot: environment now runs Python {version}, with {package} installed."
                    ),
                )
                .await;
            spawn_pyright_pin_offer(client, root);
            true
        }
        Ok(summary) => {
            report_failed_step(client, root, &summary).await;
            false
        }
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: rebuild failed. {e}"))
                .await;
            false
        }
    };
    refresh_all_buffers(shared).await;
    ok
}

/// After PyPilot has built the environment (`.venv`, or a pixi project's
/// `.pixi/envs/default`), offer to point pyright and basedpyright at
/// it (see [`pyright`]). Zed only, only when pyright is not configured yet, and
/// never without the user's click. Spawned because the toast may stay
/// unanswered, and the command that built the environment should finish now.
fn spawn_pyright_pin_offer(client: &Client, root: &Path) {
    if !CLIENT_IS_ZED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let (Some(target), Some(env)) = (pyright::pin_target(root), pyright::pin_env(root)) else {
        return;
    };
    let env = env.display();
    let client = client.clone();
    let root = root.to_path_buf();
    tokio::spawn(async move {
        let file = match target {
            pyright::PinTarget::Pyproject => "pyproject.toml ([tool.pyright])",
            pyright::PinTarget::PyrightConfig => "a new pyrightconfig.json",
        };
        let chosen = client
            .show_message_request(
                MessageType::INFO,
                format!(
                    "PyPilot: if Zed's Python language server still reports installed packages \
                     as unresolved, it is using another environment. Pin pyright/basedpyright \
                     to {env} in {file}? This overrides Zed's toolchain selection for them."
                ),
                Some(vec![action(BTN_PIN_PYRIGHT), action(BTN_NOT_NOW)]),
            )
            .await
            .ok()
            .flatten();
        if chosen.as_ref().map(|a| a.title.as_str()) != Some(BTN_PIN_PYRIGHT) {
            return;
        }
        match pyright::write_pin(&root) {
            Ok(path) => {
                client
                    .show_message(
                        MessageType::INFO,
                        format!(
                            "PyPilot: wrote the {env} pin to {}. Restart the Python language \
                             server if it does not pick it up.",
                            path.display()
                        ),
                    )
                    .await
            }
            Err(e) => {
                client
                    .show_message(
                        MessageType::WARNING,
                        format!("PyPilot: left pyright config unchanged. {e}"),
                    )
                    .await
            }
        }
    });
}

async fn report_failed_step(client: &Client, root: &Path, summary: &setup::SetupSummary) {
    if let Some(step) = summary
        .steps
        .iter()
        .find(|s| !s.ok && s.name == crate::core::pixi_manage::LOCATE_STEP)
    {
        pixi::offer_install_pixi(client, &format!("PyPilot: {}", step.detail));
        return;
    }
    let failed = summary.steps.iter().rev().find(|s| !s.ok);
    let detail = failed
        .map(|s| format!("{}: {}", s.name, s.detail))
        .unwrap_or_else(|| "the operation did not finish".to_string());
    client
        .show_message(MessageType::ERROR, format!("PyPilot: {detail}"))
        .await;
    if let Some(step) = failed {
        spawn_pixi_build_offer(client, root, &step.detail);
    }
}

/// A pip/uv build that failed on a system library conda-forge ships (see
/// [`crate::core::pixi_advice::explain_build_failure`]): say so, and offer
/// pixi. Spawned so the command that failed answers now; never in a pixi
/// project, whose own errors are explained by pixi_compat.
fn spawn_pixi_build_offer(client: &Client, root: &Path, stderr: &str) {
    if crate::core::pixi::detect(root).is_some() {
        return;
    }
    let Some(advice) = crate::core::pixi_advice::explain_build_failure(
        stderr,
        crate::matrix::conda_forge(),
        Platform::current(),
    ) else {
        return;
    };
    let client = client.clone();
    let root = root.to_path_buf();
    tokio::spawn(async move {
        let chosen = client
            .show_message_request(
                MessageType::INFO,
                format!("PyPilot: {}", advice.message),
                Some(vec![action(BTN_PIXI), action(BTN_IGNORE)]),
            )
            .await
            .ok()
            .flatten();
        if chosen.as_ref().map(|a| a.title.as_str()) == Some(BTN_PIXI) {
            run_migrate_to_pixi(&client, &root, &[]).await;
        }
    });
}

/// The advisory's "Initialize pixi project" button: show what the
/// migration would write, and write it only once the user says so.
async fn run_migrate_to_pixi(client: &Client, root: &Path, _args: &[serde_json::Value]) {
    pixi::offer_migration(client, root).await;
}

fn string_arg(args: &[serde_json::Value], index: usize) -> Option<String> {
    args.get(index)?.as_str().map(|s| s.to_string())
}

/// Every argument from `from` on, all of which must be strings. `None` when
/// there are none or one is not a string: a malformed call, not a package.
fn string_args(args: &[serde_json::Value], from: usize) -> Option<Vec<String>> {
    let rest = args.get(from..).filter(|rest| !rest.is_empty())?;
    rest.iter()
        .map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

// --- F5: onboarding and the CLI bridge ---------------------------------------

/// Poll the workspace's rescan request file. One stat per second.
async fn watch_rescan_requests(shared: Arc<Shared>, root: PathBuf) {
    let mut last_seen = rescan::request_mtime(&root);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let current = rescan::request_mtime(&root);
        if current.is_some() && current != last_seen {
            last_seen = current;
            scan_and_notify(&shared.client, &shared.source, &root, true).await;
            refresh_all_buffers(shared.clone()).await;
        }
    }
}

/// The workspace scan. `explicit` means the user asked, which lifts the silence
/// rules and turns a clean result into a confirmation instead of nothing.
async fn scan_and_notify(client: &Client, source: &PyPiClient, root: &Path, explicit: bool) {
    let command = if explicit { "scan" } else { "scan (on open)" };
    let action = crate::core::logs::Action::new("lsp", command, &[]).workspace(Some(root));
    crate::core::logs::scope(
        action,
        scan_and_notify_logged(client, source, root, explicit),
        |_| (true, None),
    )
    .await;
}

async fn scan_and_notify_logged(client: &Client, source: &PyPiClient, root: &Path, explicit: bool) {
    let settings = Settings::load(root);

    if !explicit
        && (settings.notifications == Notifications::Off
            || settings.dismissed
            || !settings.auto_check_on_open)
    {
        return;
    }

    let assessment = solver::assess(root, &settings, source).await;

    if !assessment.project.is_python_project() {
        if explicit {
            client
                .show_message(
                    MessageType::INFO,
                    "PyPilot: no Python project files found in this workspace.",
                )
                .await;
        }
        return;
    }

    // "problems-only" (the default) only raises a toast for Warning/Error, per
    // `all_good()`. "all" also surfaces Info-only results (an MPS note, a
    // conda-migration hint, a driver-matched CUDA build) instead of treating
    // them as nothing to report — that is the entire distinction the setting
    // documents; unsolicited scans never toast on Info under either tier.
    let has_something_to_say = if settings.notifications == Notifications::All {
        !assessment.findings.is_empty()
    } else {
        !assessment.all_good()
    };

    if !has_something_to_say {
        if explicit {
            client
                .show_message(MessageType::INFO, all_good_summary(&assessment))
                .await;
        }
        return;
    }

    let mut actions = vec![
        action(BTN_FIX),
        action(BTN_DETAILS),
        action(BTN_IGNORE),
        action(BTN_NEVER),
    ];
    if offers_pixi(&assessment, settings.notifications) {
        actions.insert(1, action(BTN_PIXI));
    }
    let severity = match assessment.worst_severity() {
        Some(Severity::Error) => MessageType::ERROR,
        Some(Severity::Warning) => MessageType::WARNING,
        // Only reachable under the "all" tier: an Info-only result deserves an
        // FYI toast, not one styled like a problem.
        Some(Severity::Info) | None => MessageType::INFO,
    };

    let chosen = client
        .show_message_request(severity, toast_summary(&assessment), Some(actions))
        .await
        .ok()
        .flatten();

    match chosen.as_ref().map(|a| a.title.as_str()) {
        Some(BTN_FIX) => {
            run_fix(client, source, root).await;
        }
        Some(BTN_DETAILS) => show_details(client, source, root, None).await,
        Some(BTN_NEVER) => mark_never(client, root).await,
        Some(BTN_PIXI) => run_migrate_to_pixi(client, root, &[]).await,
        _ => {}
    }
}

/// Does the toast offer "Initialize pixi project"? Only for a pixi finding
/// that is itself worth a toast: an Info one (a mixed-language repo) is
/// shown only under the "all" tier.
fn offers_pixi(a: &Assessment, notifications: Notifications) -> bool {
    a.findings.iter().any(|f| {
        f.fix == FixKind::MigrateToPixi
            && (f.severity != Severity::Info || notifications == Notifications::All)
    })
}

/// Returns whether the environment ended up set up.
async fn run_fix(client: &Client, source: &PyPiClient, root: &Path) -> bool {
    let settings = Settings::load(root);

    // A pixi project is set up by pixi, so there is no uv to offer.
    let pixi_project = crate::core::pixi::detect(root).is_some();
    let mode = if !pixi_project
        && settings.package_manager == PackageManager::Auto
        && crate::core::uv::is_system_installed().await.is_none()
    {
        // Tier B: uv is NOT on PATH, prompt user to install
        let install_actions = vec![action(BTN_INSTALL_UV), action(BTN_NO_THANKS)];
        let chosen = client
            .show_message_request(
                MessageType::INFO,
                "PyPilot: uv provides faster installs (~18 MB, installed once globally). Install it?",
                Some(install_actions),
            )
            .await
            .ok()
            .flatten();

        if chosen.as_ref().map(|a| a.title.as_str()) == Some(BTN_INSTALL_UV) {
            // Prompt 2: uv project mode vs pip-compatible mode
            let mode_actions = vec![action(BTN_MODE_UV_PROJECT), action(BTN_MODE_PIP_COMPAT)];
            let mode_chosen = client
                .show_message_request(
                    MessageType::INFO,
                    "PyPilot: How should PyPilot manage your project?",
                    Some(mode_actions),
                )
                .await
                .ok()
                .flatten();

            let target_mode = match mode_chosen.as_ref().map(|a| a.title.as_str()) {
                Some(BTN_MODE_UV_PROJECT) => ResolvedMode::UvFull,
                _ => ResolvedMode::UvPipCompat,
            };

            client
                .show_message(MessageType::INFO, "PyPilot: installing uv globally…")
                .await;

            match crate::core::uv::install_globally().await {
                Ok(info) => {
                    client
                        .show_message(
                            MessageType::INFO,
                            format!(
                                "PyPilot: uv v{} installed globally. In existing terminals, refresh PATH with: {}",
                                info.version,
                                crate::core::uv::refresh_path_command()
                            ),
                        )
                        .await;

                    if target_mode == ResolvedMode::UvPipCompat {
                        let _ = Settings::persist_package_manager(root, PackageManager::UvPip);
                    }
                    target_mode
                }
                Err(e) => {
                    client
                        .show_message(
                            MessageType::ERROR,
                            format!("PyPilot: global uv install failed ({e}), falling back to pip mode."),
                        )
                        .await;
                    ResolvedMode::PurePip
                }
            }
        } else {
            // Tier C: user declined uv install
            ResolvedMode::PurePip
        }
    } else {
        settings.resolve_mode(root).await
    };

    client
        .show_message(MessageType::INFO, "PyPilot: setting up the environment…")
        .await;

    match setup::run_with_mode(root, &settings, source, mode).await {
        Ok(summary) if summary.ok => {
            let py = summary
                .python
                .map(|p| format!("Python {p}"))
                .unwrap_or_else(|| "system interpreter".into());
            client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "PyPilot: done. {} at {}. {}",
                        py,
                        summary.venv_path.display(),
                        summary.why
                    ),
                )
                .await;
            spawn_pyright_pin_offer(client, root);
            true
        }
        Ok(summary) => {
            report_failed_step(client, root, &summary).await;
            false
        }
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: setup error. {e}"))
                .await;
            false
        }
    }
}

/// Returns whether uv is installed afterwards.
async fn run_install_uv(client: &Client) -> bool {
    if let Some(info) = crate::core::uv::is_system_installed().await {
        client
            .show_message(
                MessageType::INFO,
                format!(
                    "PyPilot: uv v{} is already installed on your system.",
                    info.version
                ),
            )
            .await;
        return true;
    }
    client
        .show_message(MessageType::INFO, "PyPilot: installing uv globally…")
        .await;
    match crate::core::uv::install_globally().await {
        Ok(info) => {
            client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "PyPilot: uv v{} installed globally. In existing terminals, refresh PATH with: {}",
                        info.version,
                        crate::core::uv::refresh_path_command()
                    ),
                )
                .await;
            true
        }
        Err(e) => {
            client
                .show_message(
                    MessageType::ERROR,
                    format!("PyPilot: global uv install failed: {e}"),
                )
                .await;
            false
        }
    }
}

async fn show_details(client: &Client, source: &PyPiClient, root: &Path, package: Option<&str>) {
    let settings = Settings::load(root);
    let assessment = solver::assess(root, &settings, source).await;
    let mut report = details_markdown(&assessment);
    if let Some(package) = package {
        report.push_str(&package_check_markdown(source, root, &assessment, package).await);
    }

    let path = std::env::temp_dir().join("pypilot-report.md");
    if std::fs::write(&path, &report).is_ok() {
        if let Ok(uri) = Url::from_file_path(&path) {
            let opened = client
                .send_request::<request::ShowDocument>(ShowDocumentParams {
                    uri,
                    external: Some(false),
                    take_focus: Some(true),
                    selection: None,
                })
                .await;
            if opened.is_ok() {
                return;
            }
        }
    }
    client.show_message(MessageType::INFO, report).await;
}

async fn mark_never(client: &Client, root: &Path) {
    if let Err(e) = Settings::mark_dismissed(root) {
        client
            .log_message(
                MessageType::WARNING,
                format!("could not persist dismissal: {e}"),
            )
            .await;
    }
}

fn action(title: &str) -> MessageActionItem {
    MessageActionItem {
        title: title.to_string(),
        properties: Default::default(),
    }
}

/// Confirmation for an explicitly requested scan that found nothing wrong.
fn all_good_summary(a: &Assessment) -> String {
    let venv = a
        .probes
        .venv
        .as_ref()
        .and_then(|v| Some((v.python?, v.kind.is_pixi())))
        .map(|(p, pixi)| {
            if pixi {
                format!("Python {p} pixi environment")
            } else {
                format!("Python {p} venv")
            }
        })
        .unwrap_or_else(|| "environment".into());
    match &a.compat {
        Some(compat) => format!(
            "PyPilot: everything looks good. The {} is compatible, and dependencies support {}.",
            venv,
            compat.intersection.to_range_string()
        ),
        None => format!("PyPilot: everything looks good. The {venv} is in place."),
    }
}

/// One line for the notification, always naming the package that caused it.
fn toast_summary(a: &Assessment) -> String {
    let primary = a
        .findings
        .iter()
        .find(|f| f.severity == Severity::Error)
        .or_else(|| a.findings.iter().find(|f| f.severity == Severity::Warning));

    match primary {
        Some(f) => format!("PyPilot: {}. {}", f.title, f.detail),
        None => "PyPilot: this project needs attention.".to_string(),
    }
}

/// Zed names itself after its release channel: `Zed`, `Zed Preview`,
/// `Zed Nightly`, `Zed Dev`.
fn is_zed_client(name: &str) -> bool {
    name == "Zed" || name.starts_with("Zed ")
}

/// Workspace root from the initialize params, folders first.
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        if let Some(first) = folders.first() {
            if let Ok(p) = first.uri.to_file_path() {
                return Some(p);
            }
        }
    }
    #[allow(deprecated)]
    if let Some(uri) = &params.root_uri {
        if let Ok(p) = uri.to_file_path() {
            return Some(p);
        }
    }
    None
}

/// Entry point for `pypilot lsp`.
pub fn run_stdio() {
    crate::core::command::hide_child_consoles();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build LSP runtime");

    rt.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::new(|client| Backend {
            shared: Arc::new(Shared {
                client,
                root: Mutex::new(None),
                documents: Mutex::new(HashMap::new()),
                revisions: Mutex::new(HashMap::new()),
                published: Mutex::new(HashMap::new()),
                source: PyPiClient::new(),
            }),
        });
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assessment_with(findings: Vec<crate::core::Finding>) -> Assessment {
        Assessment {
            probes: Default::default(),
            project: Default::default(),
            compat: None,
            target_python: None,
            findings,
        }
    }

    fn pixi_finding(severity: Severity) -> crate::core::Finding {
        crate::core::Finding {
            severity,
            title: "gdal 3.8.0 needs native libraries pip cannot install".into(),
            detail: String::new(),
            fix: FixKind::MigrateToPixi,
        }
    }

    #[test]
    fn the_pixi_button_needs_a_pixi_finding_worth_the_toast() {
        let warn = assessment_with(vec![pixi_finding(Severity::Warning)]);
        assert!(offers_pixi(&warn, Notifications::ProblemsOnly));
        let info = assessment_with(vec![pixi_finding(Severity::Info)]);
        assert!(!offers_pixi(&info, Notifications::ProblemsOnly));
        assert!(offers_pixi(&info, Notifications::All));
        assert!(!offers_pixi(
            &assessment_with(Vec::new()),
            Notifications::All
        ));
    }

    #[test]
    fn a_notebook_cell_maps_to_its_notebook_file() {
        let cell =
            Url::parse("vscode-notebook-cell:/c%3A/Users/me/proj/analysis.ipynb#W2sZmlsZQ%3D%3D")
                .unwrap();
        let notebook = notebook_url(&cell).unwrap();
        assert_eq!(
            notebook.as_str(),
            "file:///c%3A/Users/me/proj/analysis.ipynb"
        );
        assert_eq!(notebook.fragment(), None);

        #[cfg(windows)]
        assert_eq!(
            notebook.to_file_path().unwrap(),
            PathBuf::from(r"c:\Users\me\proj\analysis.ipynb")
        );

        let posix = Url::parse("vscode-notebook-cell:/home/me/my%20proj/a.ipynb#X1").unwrap();
        assert_eq!(
            notebook_url(&posix).unwrap().as_str(),
            "file:///home/me/my%20proj/a.ipynb"
        );
        #[cfg(not(windows))]
        assert_eq!(
            notebook_url(&posix).unwrap().to_file_path().unwrap(),
            PathBuf::from("/home/me/my proj/a.ipynb")
        );
    }

    #[test]
    fn cells_of_one_notebook_share_a_key() {
        let a = Url::parse("vscode-notebook-cell:/c%3A/p/a.ipynb#W1").unwrap();
        let b = Url::parse("vscode-notebook-cell:/c%3A/p/a.ipynb#W2").unwrap();
        let other = Url::parse("vscode-notebook-cell:/c%3A/p/b.ipynb#W1").unwrap();
        assert_eq!(notebook_url(&a), notebook_url(&b));
        assert_ne!(notebook_url(&a), notebook_url(&other));
    }

    #[test]
    fn only_saved_notebook_cells_have_a_notebook_file() {
        let file = Url::parse("file:///c%3A/p/main.py").unwrap();
        assert_eq!(notebook_url(&file), None);
        let untitled =
            Url::parse("vscode-notebook-cell:Untitled-1.ipynb#W0sdW50aXRsZWQ%3D").unwrap();
        assert_eq!(notebook_url(&untitled), None);
    }

    #[test]
    fn every_zed_release_channel_counts_as_zed() {
        for name in ["Zed", "Zed Preview", "Zed Nightly", "Zed Dev"] {
            assert!(is_zed_client(name), "{name}");
        }
        for name in ["Visual Studio Code", "Zedd", "neovim"] {
            assert!(!is_zed_client(name), "{name}");
        }
    }

    #[test]
    fn install_arguments_take_one_or_more_names() {
        assert_eq!(
            string_args(&[json!("numpy")], 0),
            Some(vec!["numpy".into()])
        );
        assert_eq!(
            string_args(&[json!("numpy"), json!("pandas")], 0),
            Some(vec!["numpy".into(), "pandas".into()])
        );
        // Recreate: the interpreter first, then the packages.
        let recreate = [json!("3.12"), json!("mediapipe"), json!("tflite")];
        assert_eq!(string_arg(&recreate, 0).as_deref(), Some("3.12"));
        assert_eq!(
            string_args(&recreate, 1),
            Some(vec!["mediapipe".into(), "tflite".into()])
        );

        assert_eq!(string_args(&[], 0), None);
        assert_eq!(string_args(&[json!("3.12")], 1), None);
        assert_eq!(string_args(&[json!("numpy"), json!(1)], 0), None);
    }
}

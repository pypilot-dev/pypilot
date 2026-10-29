//! The pixi LSP commands: `pypilot.installPixi` and `pypilot.migrateToPixi`.
//!
//! Both answer with JSON the VS Code extension reads:
//!
//! - `pypilot.installPixi`, no arguments: `{ "ok": bool }`. A failure is also
//!   shown with `window/showMessage`.
//! - `pypilot.migrateToPixi`, arguments `[{ "write"?: bool, "install"?: bool }]`.
//!   Without `write`: `{ "preview": "<diff>" }`; an absent preview means there
//!   is nothing to do and the reason was already shown. With `write`:
//!   `{ "written": ["<absolute path>"], "ok": bool }`.

use std::path::Path;

use serde_json::{json, Value};
use tower_lsp::lsp_types::{MessageActionItem, MessageType};
use tower_lsp::Client;

use crate::core::pixi_install;
use crate::core::pixi_migrate::{self, MigrateOptions};
use crate::settings::Settings;

pub const CMD_INSTALL_PIXI: &str = "pypilot.installPixi";
pub const CMD_MIGRATE_TO_PIXI: &str = "pypilot.migrateToPixi";

/// For `executeCommandProvider.commands`.
pub const COMMANDS: &[&str] = &[CMD_INSTALL_PIXI, CMD_MIGRATE_TO_PIXI];

const BTN_INSTALL_PIXI: &str = "Install pixi";
const BTN_NOT_NOW: &str = "Not now";
const BTN_WRITE_INSTALL: &str = "Write and install";
const BTN_WRITE: &str = "Write pixi.toml";
const BTN_CANCEL: &str = "Cancel";

/// The advisory's "Initialize pixi project" button, which Zed and the
/// notification path use: nothing there can show a returned preview, so say
/// what the migration would write and write it only once the user agrees.
pub async fn offer_migration(client: &Client, root: &Path) {
    let settings = Settings::load(root);
    let dry = match pixi_migrate::migrate_to_pixi(root, &settings, MigrateOptions::default()).await
    {
        Ok(summary) => summary,
        Err(e) => {
            client
                .show_message(MessageType::WARNING, format!("PyPilot: {e:#}"))
                .await;
            return;
        }
    };
    let message = format!(
        "PyPilot: {} pyproject.toml and requirements.txt stay as they are; `pypilot migrate-to-pixi` shows the full preview.",
        dry.headline()
    );
    let actions = [BTN_WRITE_INSTALL, BTN_WRITE, BTN_CANCEL]
        .iter()
        .map(|title| MessageActionItem {
            title: title.to_string(),
            properties: Default::default(),
        })
        .collect();
    let chosen = client
        .show_message_request(MessageType::INFO, message, Some(actions))
        .await
        .ok()
        .flatten();
    let install = match chosen.as_ref().map(|a| a.title.as_str()) {
        Some(BTN_WRITE_INSTALL) => true,
        Some(BTN_WRITE) => false,
        _ => return,
    };
    migrate_to_pixi(
        client,
        root,
        &[json!({ "write": true, "install": install })],
    )
    .await;
}

/// Run `command` if it is one of [`COMMANDS`]; `None` for any other.
pub async fn execute(command: &str, client: &Client, root: &Path, args: &[Value]) -> Option<Value> {
    match command {
        CMD_INSTALL_PIXI => Some(json!({ "ok": install_pixi(client).await })),
        CMD_MIGRATE_TO_PIXI => Some(migrate_to_pixi(client, root, args).await),
        _ => None,
    }
}

/// Install pixi with its official installer; whether pixi works afterwards.
pub async fn install_pixi(client: &Client) -> bool {
    client
        .show_message(MessageType::INFO, "PyPilot: installing pixi…")
        .await;
    match pixi_install::install().await {
        Ok(p) if p.already_installed => {
            client
                .show_message(
                    MessageType::INFO,
                    format!("PyPilot: pixi {} is already installed.", p.version),
                )
                .await;
            true
        }
        Ok(p) => {
            client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "PyPilot: pixi {} installed at {}. In terminals opened earlier, refresh PATH with: {}",
                        p.version,
                        p.path.display(),
                        pixi_install::refresh_path_command()
                    ),
                )
                .await;
            true
        }
        Err(e) => {
            client
                .show_message(
                    MessageType::ERROR,
                    format!(
                        "PyPilot: installing pixi failed: {e:#}. To install it by hand: {}",
                        pixi_install::manual_command()
                    ),
                )
                .await;
            false
        }
    }
}

/// `pypilot.migrateToPixi`, as described in the module docs.
pub async fn migrate_to_pixi(client: &Client, root: &Path, args: &[Value]) -> Value {
    let flag = |key: &str| {
        args.first()
            .and_then(|a| a.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let opts = MigrateOptions {
        write: flag("write"),
        install: flag("write") && flag("install"),
        ..MigrateOptions::default()
    };
    let write = opts.write;
    let settings = Settings::load(root);
    let summary = match pixi_migrate::migrate_to_pixi(root, &settings, opts).await {
        Ok(s) => s,
        Err(e) => {
            client
                .show_message(MessageType::WARNING, format!("PyPilot: {e:#}"))
                .await;
            return if write {
                json!({ "written": [], "ok": false })
            } else {
                json!({})
            };
        }
    };
    if !write {
        return json!({ "preview": summary.preview });
    }

    crate::core::rescan::notify(root, crate::core::rescan::Kind::Setup);
    let mut message = format!("PyPilot: {}", summary.headline());
    if let Some(install) = &summary.install {
        match install.steps.iter().rev().find(|s| !s.ok) {
            Some(failed) if install.ok => {
                message.push_str(&format!(" {}: {}", failed.name, failed.detail))
            }
            Some(failed) => message.push_str(&format!(
                " pixi install did not finish ({}: {}).",
                failed.name, failed.detail
            )),
            None => message.push_str(" The pixi environment is installed."),
        }
    } else if let Some(next) = &summary.next_command {
        message.push_str(&format!(" Next: {next}."));
    }
    let level = if summary.ok() {
        MessageType::INFO
    } else {
        MessageType::ERROR
    };
    client.show_message(level, message).await;
    let missing_pixi = summary.install.as_ref().is_some_and(|i| {
        i.steps
            .iter()
            .any(|s| !s.ok && s.name == crate::core::pixi_manage::LOCATE_STEP)
    });
    if missing_pixi {
        offer_install_pixi(
            client,
            "PyPilot: pixi is not installed, so the environment was not created. Install pixi now?",
        );
    }
    json!({
        "written": [summary.manifest_path.to_string_lossy()],
        "ok": summary.ok(),
    })
}

/// Offer pixi's installer in a toast of its own, without holding up the
/// caller: a toast waits on the user for as long as it stays open.
pub fn offer_install_pixi(client: &Client, message: &str) {
    let client = client.clone();
    let message = message.to_string();
    tokio::spawn(async move {
        let actions = vec![
            MessageActionItem {
                title: BTN_INSTALL_PIXI.to_string(),
                properties: Default::default(),
            },
            MessageActionItem {
                title: BTN_NOT_NOW.to_string(),
                properties: Default::default(),
            },
        ];
        let chosen = client
            .show_message_request(MessageType::WARNING, message, Some(actions))
            .await
            .ok()
            .flatten();
        if chosen.is_some_and(|a| a.title == BTN_INSTALL_PIXI) {
            install_pixi(&client).await;
        }
    });
}

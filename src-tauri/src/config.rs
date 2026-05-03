//! Persistent agent config — stored as plain JSON in the OS-canonical config
//! dir. We keep the format identical to what the old Bun CLI used so users
//! migrating from v0.1 don't need to re-paste their token.
//!
//! Linux:   ~/.config/lesscommerce/print-agent.json
//! macOS:   ~/Library/Application Support/io.lesscommerce.print-agent/print-agent.json
//! Windows: %APPDATA%\lesscommerce\print-agent.json

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Base URL of the LessCommerce backend, e.g. `https://api.lesscommerce.io`.
    pub api_url: String,
    /// Bearer token issued by the admin panel. Format `lpa_live_<48 chars>`.
    pub printer_token: String,
    /// Operating system printer name to forward the bytes to (`Zebra ZD220`,
    /// `Brother_QL_700`, …). Resolved against `lp -e` / `Get-Printer` at setup.
    pub system_printer: String,
    /// Friendly name shown in the LessCommerce admin panel. Defaults to the
    /// printer's name field returned by the heartbeat.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Polling cadence in seconds. The server can override this per heartbeat
    /// — this value is just the bootstrap before the first heartbeat lands.
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,
    /// Whether to launch the agent automatically when the user logs in.
    /// Stored here so we can restore the toggle on the setup UI; the actual
    /// registration happens via `tauri-plugin-autostart`.
    #[serde(default)]
    pub launch_at_login: bool,
}

fn default_poll_interval() -> u64 {
    5
}

impl AgentConfig {
    pub fn config_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("io", "lesscommerce", "print-agent")
            .context("Could not resolve OS config dir for print-agent")?;
        Ok(dirs.config_dir().join("print-agent.json"))
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let cfg = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse {} — is the JSON valid?", path.display()))?;
        Ok(Some(cfg))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }

    /// Sanity check used by the polling loop on startup — refuses to run with
    /// half-configured state instead of spamming 401s at the API.
    pub fn is_complete(&self) -> bool {
        !self.api_url.trim().is_empty()
            && !self.printer_token.trim().is_empty()
            && !self.system_printer.trim().is_empty()
    }
}

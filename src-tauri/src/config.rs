//! Persistent agent config — stored as plain JSON in the OS-canonical config
//! dir.
//!
//! Linux:   ~/.config/lesscommerce/print-agent.json
//! macOS:   ~/Library/Application Support/io.lesscommerce.print-agent/print-agent.json
//! Windows: %APPDATA%\lesscommerce\print-agent.json
//!
//! Od v0.4 jeden agent obsługuje kilka drukarek: każda to osobny wpis w
//! `printers` z własnym tokenem (osobna drukarka w panelu) i własną drukarką
//! systemową. Stary plik z jedną drukarką (`printer_token` + `system_printer`
//! na górnym poziomie) jest przenoszony do listy przy pierwszym odczycie.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrinterSlot {
    /// Base URL of the LessCommerce backend, e.g. `https://api.lesscommerce.io`.
    pub api_url: String,
    /// Bearer token issued by the admin panel. Format `lpa_live_<48 chars>`.
    pub printer_token: String,
    /// Operating system printer name to forward the bytes to. Empty right
    /// after pairing, until the user picks one.
    #[serde(default)]
    pub system_printer: String,
    /// Friendly name from the panel.
    #[serde(default)]
    pub display_name: Option<String>,
}

impl PrinterSlot {
    /// Stable local id — start of the token. Never sent anywhere.
    pub fn id(&self) -> String {
        self.printer_token.chars().take(20).collect()
    }

    pub fn is_complete(&self) -> bool {
        !self.api_url.trim().is_empty()
            && !self.printer_token.trim().is_empty()
            && !self.system_printer.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    #[serde(default)]
    pub printers: Vec<PrinterSlot>,
    /// Polling cadence in seconds. The server can override this per heartbeat
    /// — this value is just the bootstrap before the first heartbeat lands.
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,
    /// Whether to launch the agent automatically when the user logs in.
    #[serde(default)]
    pub launch_at_login: bool,

    // ---- v0.3 single-printer fields, read once and moved into `printers` ----
    #[serde(default, skip_serializing)]
    api_url: Option<String>,
    #[serde(default, skip_serializing)]
    printer_token: Option<String>,
    #[serde(default, skip_serializing)]
    system_printer: Option<String>,
    #[serde(default, skip_serializing)]
    display_name: Option<String>,
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
        let mut cfg: Self = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse {} — is the JSON valid?", path.display()))?;
        if cfg.migrate_legacy() {
            let _ = cfg.save();
        }
        Ok(Some(cfg))
    }

    pub fn load_or_default() -> Self {
        Self::load().ok().flatten().unwrap_or(Self {
            poll_interval: default_poll_interval(),
            ..Default::default()
        })
    }

    /// Przenosi stary zapis jednej drukarki do listy. Zwraca true, gdy coś zmienił.
    fn migrate_legacy(&mut self) -> bool {
        let token = self.printer_token.take().unwrap_or_default();
        let api = self.api_url.take().unwrap_or_default();
        let system = self.system_printer.take().unwrap_or_default();
        let name = self.display_name.take();
        if token.is_empty() || api.is_empty() {
            return false;
        }
        if !self.printers.iter().any(|p| p.printer_token == token) {
            self.printers.push(PrinterSlot {
                api_url: api,
                printer_token: token,
                system_printer: system,
                display_name: name,
            });
        }
        true
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

    /// Dodaje świeżo sparowaną drukarkę (albo odświeża token, jeśli już jest).
    pub fn upsert_pairing(&mut self, slot: PrinterSlot) {
        if let Some(existing) = self
            .printers
            .iter_mut()
            .find(|p| p.printer_token == slot.printer_token)
        {
            existing.api_url = slot.api_url;
            existing.display_name = slot.display_name;
        } else {
            self.printers.push(slot);
        }
    }

    /// Drukarka sparowana, ale jeszcze bez drukarki systemowej.
    pub fn pending(&self) -> Option<&PrinterSlot> {
        self.printers
            .iter()
            .find(|p| p.system_printer.trim().is_empty())
    }

    pub fn complete_printers(&self) -> Vec<PrinterSlot> {
        self.printers
            .iter()
            .filter(|p| p.is_complete())
            .cloned()
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.printers.iter().any(|p| p.is_complete())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_single_printer_moves_into_list() {
        let json = r#"{"api_url":"https://api.x","printer_token":"lpa_live_abc","system_printer":"Zebra","display_name":"Magazyn","poll_interval":5,"launch_at_login":true}"#;
        let mut cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.migrate_legacy());
        assert_eq!(cfg.printers.len(), 1);
        assert_eq!(cfg.printers[0].system_printer, "Zebra");
        assert!(cfg.is_complete());
        let saved = serde_json::to_string(&cfg).unwrap();
        assert!(!saved.contains("\"printer_token\":\"lpa_live_abc\",\"system_printer\":\"Zebra\",\"display_name\":\"Magazyn\",\"poll_interval\""));
        assert!(!cfg.migrate_legacy());
    }

    #[test]
    fn pairing_adds_second_printer_and_pending_is_found() {
        let mut cfg = AgentConfig::default();
        let slot = |t: &str, sp: &str| PrinterSlot {
            api_url: "https://api.x".into(),
            printer_token: t.into(),
            system_printer: sp.into(),
            display_name: None,
        };
        cfg.upsert_pairing(slot("lpa_live_one", "Zebra"));
        cfg.upsert_pairing(slot("lpa_live_two", ""));
        assert_eq!(cfg.printers.len(), 2);
        assert_eq!(cfg.pending().unwrap().printer_token, "lpa_live_two");
        assert_eq!(cfg.complete_printers().len(), 1);
        cfg.upsert_pairing(slot("lpa_live_one", ""));
        assert_eq!(
            cfg.printers.len(),
            2,
            "ponowne parowanie tej samej drukarki nie dubluje wpisu"
        );
        assert_eq!(cfg.printers[0].system_printer, "Zebra");
    }
}

//! Printer discovery and label dispatch.
//!
//! Discovery:
//!   - Linux/macOS: `lpstat -p` enumerates CUPS queues.
//!   - Windows: PowerShell `Get-Printer` enumerates installed printers.
//!
//! Sending:
//!   - PDF on macOS/Linux: `lp -d <printer> <file>` (CUPS handles rendering).
//!   - PDF on Windows: SumatraPDF `-print-to <printer> -silent`. Falls back
//!     to the default PDF handler via `Start-Process -Verb PrintTo` when
//!     SumatraPDF isn't on PATH (warns the user but works for most setups).
//!   - ZPL/EPL: `lp -o raw` on Unix, `Out-Printer` on Windows. Bytes go to
//!     the printer untouched.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use tempfile::NamedTempFile;
use tokio::process::Command;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemPrinter {
    pub name: String,
}

pub async fn list_system_printers() -> Result<Vec<SystemPrinter>> {
    if cfg!(target_os = "windows") {
        list_windows().await
    } else {
        list_cups().await
    }
}

async fn list_cups() -> Result<Vec<SystemPrinter>> {
    let output = Command::new("lpstat")
        .arg("-p")
        .output()
        .await
        .context("Failed to run `lpstat -p` — is CUPS installed?")?;
    if !output.status.success() {
        return Err(anyhow!(
            "lpstat exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let names = stdout
        .lines()
        .filter_map(|line| {
            // Lines look like: "printer Zebra_ZD220 is idle. enabled since …"
            line.strip_prefix("printer ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|name| SystemPrinter {
                    name: name.to_string(),
                })
        })
        .collect();
    Ok(names)
}

async fn list_windows() -> Result<Vec<SystemPrinter>> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Printer | Select-Object -ExpandProperty Name",
        ])
        .output()
        .await
        .context("Failed to run PowerShell Get-Printer")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Get-Printer exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let names = stdout
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| SystemPrinter {
            name: s.to_string(),
        })
        .collect();
    Ok(names)
}

/// Dispatch the label bytes to the system printer. Returns elapsed time so
/// the caller can ack with a duration metric. The temp file is auto-deleted
/// when the `NamedTempFile` drops at the end of the scope.
pub async fn print_label(bytes: &[u8], format: &str, system_printer: &str) -> Result<u128> {
    let extension = if format.eq_ignore_ascii_case("pdf") {
        "pdf"
    } else {
        "prn"
    };

    let mut tmp = NamedTempFile::with_suffix(format!(".{}", extension).as_str())
        .context("Failed to create temp file for label")?;
    tmp.write_all(bytes)
        .context("Failed to write label bytes to temp file")?;
    tmp.flush().ok();

    let started = Instant::now();
    if cfg!(target_os = "windows") {
        send_windows(tmp.path(), format, system_printer).await?;
    } else {
        send_cups(tmp.path(), format, system_printer).await?;
    }
    Ok(started.elapsed().as_millis())
}

async fn send_cups(file: &Path, format: &str, printer: &str) -> Result<()> {
    let mut cmd = Command::new("lp");
    cmd.arg("-d").arg(printer);
    if format.eq_ignore_ascii_case("zpl") || format.eq_ignore_ascii_case("epl") {
        cmd.args(["-o", "raw"]);
    }
    cmd.arg(file);
    let output = cmd.output().await.context("Failed to run `lp`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "lp exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

async fn send_windows(file: &Path, format: &str, printer: &str) -> Result<()> {
    if format.eq_ignore_ascii_case("pdf") {
        // First try SumatraPDF — the canonical "silent print" tool for
        // Windows label workflows. Quoted args because printer names often
        // contain spaces ("Zebra ZD220 (USB)").
        let sumatra = std::env::var("SUMATRA_PDF").unwrap_or_else(|_| "SumatraPDF.exe".to_string());
        let attempt = Command::new(&sumatra)
            .args([
                "-print-to",
                printer,
                "-silent",
                file.to_str().unwrap_or_default(),
            ])
            .output()
            .await;
        match attempt {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                log::warn!(
                    "SumatraPDF returned non-zero ({}); falling back to Start-Process. stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(err) => {
                log::warn!(
                    "SumatraPDF not invokable ({err}); falling back to Start-Process. \
                     Install SumatraPDF for reliable silent label printing."
                );
            }
        }
        // Fallback — uses whatever is registered as default `.pdf` handler.
        // Slower, sometimes shows a tiny print window.
        let path = file.to_string_lossy().replace('\'', "''");
        let printer_escaped = printer.replace('\'', "''");
        let cmd = format!(
            "Start-Process -FilePath '{path}' -Verb PrintTo -ArgumentList '\"{printer}\"' \
             -WindowStyle Hidden -Wait",
            path = path,
            printer = printer_escaped
        );
        let output = Command::new("powershell")
            .args(["-NoProfile", "-Command", &cmd])
            .output()
            .await
            .context("Failed to run PowerShell Start-Process fallback")?;
        if !output.status.success() {
            return Err(anyhow!(
                "PrintTo fallback exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        return Ok(());
    }

    // ZPL / EPL — raw bytes via Out-Printer.
    let path = file.to_string_lossy().replace('\'', "''");
    let printer_escaped = printer.replace('\'', "''");
    let cmd = format!(
        "Get-Content -Encoding Byte -Path '{path}' | Out-Printer -Name '{printer}'",
        path = path,
        printer = printer_escaped
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &cmd])
        .output()
        .await
        .context("Failed to run PowerShell Out-Printer")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Out-Printer exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

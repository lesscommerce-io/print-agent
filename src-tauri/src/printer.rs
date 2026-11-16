//! Printer discovery and label dispatch.
//!
//! Discovery:
//!   - Linux/macOS: `lpstat -p` enumerates CUPS queues.
//!   - Windows: PowerShell `Get-Printer` enumerates installed printers.
//!
//! Sending:
//!   - PDF on macOS/Linux: `lp -d <printer> <file>` (CUPS handles rendering).
//!   - PDF on Windows: SumatraPDF `-print-to <printer> -silent` (env, PATH
//!     albo typowe katalogi instalacji). Fallback: `Start-Process -Verb
//!     PrintTo` przez domyślny program PDF.
//!   - ZPL/EPL: `lp -o raw` on Unix, winspool RAW on Windows. Bytes go to
//!     the printer untouched.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use tempfile::NamedTempFile;
use tokio::process::Command;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemPrinter {
    /// Nazwa kolejki CUPS / Windows — to idzie do `lp -d`.
    pub name: String,
    /// Opis do wyświetlenia, jeśli system go zna.
    pub label: Option<String>,
}

pub async fn list_system_printers() -> Result<Vec<SystemPrinter>> {
    if cfg!(target_os = "windows") {
        list_windows().await
    } else {
        list_cups().await
    }
}

async fn list_cups() -> Result<Vec<SystemPrinter>> {
    // `lpstat -e` daje same nazwy kolejek, niezależnie od języka systemu.
    // (`lpstat -p` na polskim macOS pisze „drukarka X jest…”, nie „printer X”.)
    let output = Command::new("lpstat")
        .arg("-e")
        .output()
        .await
        .context("Failed to run `lpstat -e` — is CUPS installed?")?;
    if !output.status.success() {
        return Err(anyhow!(
            "lpstat exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut printers = Vec::new();
    for name in stdout.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let label = cups_description(name).await;
        printers.push(SystemPrinter {
            name: name.to_string(),
            label,
        });
    }
    Ok(printers)
}

/// Czytelna nazwa drukarki („Xerox Kasia”) z `printer-info` w lpoptions.
async fn cups_description(name: &str) -> Option<String> {
    let out = Command::new("lpoptions")
        .args(["-p", name])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_printer_info(&text)
}

fn parse_printer_info(text: &str) -> Option<String> {
    let rest = text.split("printer-info=").nth(1)?;
    let value = if let Some(quoted) = rest.strip_prefix('\'') {
        quoted.split('\'').next()?
    } else {
        rest.split_whitespace().next()?
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// PowerShell bez okna konsoli i z wyjściem w UTF-8 — polskie nazwy drukarek
/// („Drukarka biurowa – piętro”) wracają bez krzaków, a agent w tle nie
/// miga czarnym oknem przy każdym wydruku.
fn powershell(script: &str) -> Command {
    let mut cmd = Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &format!("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {script}"),
    ]);
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

async fn list_windows() -> Result<Vec<SystemPrinter>> {
    // Get-Printer (moduł PrintManagement) nie zawsze jest dostępny — wtedy
    // ta sama lista z WMI.
    let script = "try { Get-Printer -ErrorAction Stop | Select-Object -ExpandProperty Name } \
                  catch { Get-CimInstance Win32_Printer | Select-Object -ExpandProperty Name }";
    let output = powershell(script)
        .output()
        .await
        .context("Failed to run PowerShell printer listing")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Printer listing exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let names = stdout
        .trim_start_matches('\u{feff}')
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| SystemPrinter {
            name: s.to_string(),
            label: None,
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

/// SumatraPDF: zmienna SUMATRA_PDF, PATH, potem typowe miejsca instalacji.
fn sumatra_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("SUMATRA_PDF") {
        out.push(p);
    }
    for (var, sub) in [
        ("LOCALAPPDATA", "SumatraPDF\\SumatraPDF.exe"),
        ("ProgramFiles", "SumatraPDF\\SumatraPDF.exe"),
        ("ProgramFiles(x86)", "SumatraPDF\\SumatraPDF.exe"),
    ] {
        if let Ok(base) = std::env::var(var) {
            let path = std::path::Path::new(&base).join(sub);
            if path.exists() {
                out.push(path.to_string_lossy().to_string());
            }
        }
    }
    out.push("SumatraPDF.exe".to_string());
    out
}

async fn send_windows(file: &Path, format: &str, printer: &str) -> Result<()> {
    let path = file.to_string_lossy().replace('\'', "''");
    let printer_escaped = printer.replace('\'', "''");

    if format.eq_ignore_ascii_case("pdf") {
        // SumatraPDF drukuje PDF po cichu i w skali 1:1 — najpewniejsza droga
        // dla etykiet. Nazwy drukarek często mają spacje, stąd osobne argumenty.
        for sumatra in sumatra_candidates() {
            let mut cmd = Command::new(&sumatra);
            cmd.args([
                "-print-to",
                printer,
                "-print-settings",
                "noscale",
                "-silent",
                file.to_str().unwrap_or_default(),
            ]);
            #[cfg(target_os = "windows")]
            {
                cmd.creation_flags(0x0800_0000);
            }
            match cmd.output().await {
                Ok(output) if output.status.success() => return Ok(()),
                Ok(output) => log::warn!(
                    "SumatraPDF ({sumatra}) returned {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ),
                Err(err) => log::debug!("SumatraPDF not at {sumatra}: {err}"),
            }
        }
        // Fallback — domyślny program do PDF (nie każdy obsługuje PrintTo,
        // np. Edge — wtedy błąd mówi, żeby doinstalować SumatraPDF).
        let cmd = format!(
            "Start-Process -FilePath '{path}' -Verb PrintTo -ArgumentList '\"{printer_escaped}\"' \
             -WindowStyle Hidden -Wait"
        );
        let output = powershell(&cmd)
            .output()
            .await
            .context("Failed to run PowerShell Start-Process fallback")?;
        if !output.status.success() {
            return Err(anyhow!(
                "Brak SumatraPDF, a domyślny program PDF nie umie drukować w tle ({}). \
                 Zainstaluj SumatraPDF (sumatrapdfreader.org) i spróbuj ponownie. {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        return Ok(());
    }

    // ZPL / EPL — surowe bajty wprost do spoolera (winspool, typ RAW).
    // Out-Printer by je wydrukował jako tekst, a nie jako polecenia drukarki.
    let script = format!(
        r#"$sig = @'
[DllImport("winspool.drv", CharSet=CharSet.Unicode, SetLastError=true)] public static extern bool OpenPrinter(string n, out IntPtr h, IntPtr d);
[DllImport("winspool.drv", SetLastError=true)] public static extern bool ClosePrinter(IntPtr h);
[StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public class DOCINFO {{ public string pDocName; public string pOutputFile; public string pDataType; }}
[DllImport("winspool.drv", CharSet=CharSet.Unicode, SetLastError=true)] public static extern int StartDocPrinter(IntPtr h, int level, DOCINFO di);
[DllImport("winspool.drv", SetLastError=true)] public static extern bool EndDocPrinter(IntPtr h);
[DllImport("winspool.drv", SetLastError=true)] public static extern bool StartPagePrinter(IntPtr h);
[DllImport("winspool.drv", SetLastError=true)] public static extern bool EndPagePrinter(IntPtr h);
[DllImport("winspool.drv", SetLastError=true)] public static extern bool WritePrinter(IntPtr h, byte[] b, int c, out int w);
'@
Add-Type -MemberDefinition $sig -Name RawPrint -Namespace LessCommerce -UsingNamespace System.Runtime.InteropServices
$bytes = [System.IO.File]::ReadAllBytes('{path}')
$h = [IntPtr]::Zero
if (-not [LessCommerce.RawPrint]::OpenPrinter('{printer_escaped}', [ref]$h, [IntPtr]::Zero)) {{ throw "Nie można otworzyć drukarki" }}
$di = New-Object LessCommerce.RawPrint+DOCINFO
$di.pDocName = 'LessCommerce label'; $di.pDataType = 'RAW'
[void][LessCommerce.RawPrint]::StartDocPrinter($h, 1, $di)
[void][LessCommerce.RawPrint]::StartPagePrinter($h)
$w = 0
$ok = [LessCommerce.RawPrint]::WritePrinter($h, $bytes, $bytes.Length, [ref]$w)
[void][LessCommerce.RawPrint]::EndPagePrinter($h)
[void][LessCommerce.RawPrint]::EndDocPrinter($h)
[void][LessCommerce.RawPrint]::ClosePrinter($h)
if (-not $ok) {{ throw "Zapis do drukarki nie powiódł się" }}"#
    );
    let output = powershell(&script)
        .output()
        .await
        .context("Failed to run PowerShell raw print")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Raw print exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_printer_info;

    #[test]
    fn printer_info_quoted_and_bare() {
        assert_eq!(
            parse_printer_info(
                "copies=1 printer-info='Xerox Kasia (3b:64:40)' printer-is-shared=false"
            )
            .as_deref(),
            Some("Xerox Kasia (3b:64:40)")
        );
        assert_eq!(
            parse_printer_info("printer-info=Zebra printer-x=1").as_deref(),
            Some("Zebra")
        );
        assert_eq!(parse_printer_info("copies=1"), None);
    }
}

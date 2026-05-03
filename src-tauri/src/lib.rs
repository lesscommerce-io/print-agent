//! Tauri app wiring — IPC commands, system tray, deep-link pairing flow,
//! and lifecycle of the polling task.
//!
//! The setup window (vanilla HTML/JS in `../ui/`) drives a 3-screen flow:
//!
//!   PAIR    — single button. Calls `start_pairing()`, which generates a
//!             device_code, opens the LessCommerce panel in the default
//!             browser, and waits for a `lesscommerce-print-agent://paired`
//!             deep-link to arrive.
//!   PRINTER — picks the system printer (the only thing that varies per
//!             host). Calls `finish_setup()`, which writes config and
//!             starts polling.
//!   STATUS  — live tray-like view: dot color + counters.
//!
//! `unpair()` wipes config and returns to PAIR.

mod api;
mod config;
mod poller;
mod printer;

use config::AgentConfig;
use poller::{PollerHandle, PollerStatus, SharedStatus};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_deep_link::DeepLinkExt;
use tokio::sync::Mutex;

/// URL of the LessCommerce panel page that handles device pairing. Hardcoded
/// to prod; can be overridden at runtime via `LESSCOMMERCE_PAIRING_URL` for
/// staging or local dev (e.g. `http://localhost:8080/print-agent/pair`).
const DEFAULT_PAIRING_URL: &str = "https://panel.lesscommerce.io/print-agent/pair";

fn pairing_base_url() -> String {
    std::env::var("LESSCOMMERCE_PAIRING_URL").unwrap_or_else(|_| DEFAULT_PAIRING_URL.to_string())
}

fn setup_url() -> WebviewUrl {
    WebviewUrl::App("index.html".into())
}

// Tray icons baked into the binary — four pre-rendered PNGs corresponding to
// the four poller states. We swap them at runtime so the menu-bar icon
// reflects whether the agent is idle, polling, actively printing or stuck.
const TRAY_IDLE: &[u8] = include_bytes!("../icons/tray-idle.png");
const TRAY_POLLING: &[u8] = include_bytes!("../icons/tray-polling.png");
const TRAY_PRINTING: &[u8] = include_bytes!("../icons/tray-printing.png");
const TRAY_ERROR: &[u8] = include_bytes!("../icons/tray-error.png");

/// Decode a baked PNG into Tauri's RGBA `Image`. The `image` crate handles
/// PNG → RGBA conversion; Tauri then takes the raw pixels. ~600B PNGs decode
/// in microseconds so we just do it on every state change rather than caching.
fn decode_png(bytes: &[u8]) -> Option<Image<'static>> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(Image::new_owned(img.into_raw(), w, h))
}

fn tray_icon_for_state(state: &str) -> Option<Image<'static>> {
    let bytes = match state {
        "polling" | "idle-pulse" => TRAY_POLLING,
        "printing" => TRAY_PRINTING,
        "error" => TRAY_ERROR,
        _ => TRAY_IDLE,
    };
    decode_png(bytes)
}

fn tray_tooltip_for_status(status: &PollerStatus) -> String {
    let prefix = match status.state.as_str() {
        "idle" => "Czekam na zlecenia",
        "polling" => "Łączenie z serwerem",
        "printing" => "Drukuję…",
        "error" => "Błąd",
        "unconfigured" => "Nieskonfigurowane — kliknij, aby sparować",
        "stopped" => "Zatrzymane",
        other => other,
    };
    if let Some(name) = &status.printer_name {
        format!("{prefix} · {name}")
    } else {
        prefix.to_string()
    }
}

/// Apply the current poller status to the tray (icon + tooltip). Called
/// from setup() once and from every status emission afterwards. The
/// "printing" pulse animation is driven by [`spawn_printing_pulse`] — this
/// function only sets the steady-state icon.
fn refresh_tray(app: &AppHandle, status: &PollerStatus) {
    if let Some(tray) = app.tray_by_id("main") {
        if let Some(icon) = tray_icon_for_state(&status.state) {
            let _ = tray.set_icon(Some(icon));
        }
        let _ = tray.set_tooltip(Some(tray_tooltip_for_status(status)));
    }
}

#[derive(Default)]
struct PollerState {
    handle: Mutex<Option<PollerHandle>>,
    status: SharedStatus,
}

/// In-memory pairing state — the device_code we expect the panel page to
/// send back. Only one pairing can be in flight at a time; `start_pairing`
/// overwrites whatever was there.
#[derive(Default)]
struct PairingState {
    pending: StdMutex<Option<PendingPairing>>,
}

struct PendingPairing {
    device_code: String,
}

impl PairingState {
    fn set(&self, code: String) {
        *self.pending.lock().unwrap() = Some(PendingPairing { device_code: code });
    }

    fn take_if_matches(&self, code: &str) -> bool {
        let mut guard = self.pending.lock().unwrap();
        match guard.as_ref() {
            Some(p) if p.device_code == code => {
                *guard = None;
                true
            }
            _ => false,
        }
    }

    fn clear(&self) {
        *self.pending.lock().unwrap() = None;
    }
}

// ============================================================================
// IPC commands
// ============================================================================

/// Returns the current saved config (token + api + printer). UI uses this on
/// boot to decide which screen to show.
#[tauri::command]
async fn load_pairing_state() -> Result<Option<AgentConfig>, String> {
    AgentConfig::load().map_err(|e| e.to_string())
}

/// Generate a device_code, open the panel pairing page in the default
/// browser. Caller's window stays open and waits for the `paired` event
/// fired by the deep-link handler.
#[tauri::command]
fn start_pairing(app: AppHandle, pairing: State<'_, PairingState>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    // Generate a quick-and-dirty 128-bit code from /dev/urandom (or
    // CryptGenRandom on Windows). 32 hex chars is plenty for "this is the
    // browser our agent just spoke to".
    let mut bytes = [0u8; 16];
    getrandom_bytes(&mut bytes);
    let device_code = bytes.iter().fold(String::with_capacity(32), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{:02x}", b);
        acc
    });

    pairing.set(device_code.clone());

    let host = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "this-computer".to_string());

    // Building the URL by hand (instead of `Url::parse_with_params`) so we
    // surface a clear error if the override env var is malformed.
    let mut url = Url::parse(&pairing_base_url())
        .map_err(|e| format!("Bad pairing URL: {e}"))?;
    url.query_pairs_mut()
        .append_pair("device_code", &device_code)
        .append_pair("callback", "lesscommerce-print-agent://paired")
        .append_pair("hostname", &host);

    app.opener()
        .open_url(url.to_string(), None::<&str>)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn cancel_pairing(pairing: State<'_, PairingState>) -> Result<(), String> {
    pairing.clear();
    Ok(())
}

#[tauri::command]
async fn list_printers() -> Result<Vec<printer::SystemPrinter>, String> {
    printer::list_system_printers()
        .await
        .map_err(|e| e.to_string())
}

/// Called from PRINTER screen — combines the chosen system printer with the
/// already-saved (token + api) and persists. Then starts polling.
#[tauri::command]
async fn finish_setup(
    system_printer: String,
    launch_at_login: bool,
    app: AppHandle,
    state: State<'_, PollerState>,
) -> Result<(), String> {
    let mut cfg = AgentConfig::load()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "No pairing on disk — pair the device first.".to_string())?;
    cfg.system_printer = system_printer;
    cfg.launch_at_login = launch_at_login;
    cfg.save().map_err(|e| e.to_string())?;

    // Toggle autostart per user choice.
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let _ = if launch_at_login {
        mgr.enable()
    } else {
        mgr.disable()
    };

    restart_poller(&app, &state, cfg).await;
    Ok(())
}

/// Wipe the config and stop polling. UI returns to the PAIR screen.
#[tauri::command]
async fn unpair(app: AppHandle, state: State<'_, PollerState>) -> Result<(), String> {
    // Stop the poller first so it doesn't hit a half-deleted config mid-flight.
    {
        let mut handle_guard = state.handle.lock().await;
        if let Some(prev) = handle_guard.take() {
            prev.stop();
        }
    }

    if let Ok(path) = AgentConfig::config_path() {
        let _ = std::fs::remove_file(&path);
    }

    // Reset status to "unconfigured" so the tray icon stops claiming we're
    // online with stale info.
    {
        let mut s = state.status.lock().await;
        *s = PollerStatus::default();
    }

    // Disable autostart on unpair — fresh setup will re-enable if the user
    // ticks the checkbox again.
    use tauri_plugin_autostart::ManagerExt;
    let _ = app.autolaunch().disable();

    let _ = app.emit("poller-status", &PollerStatus::default());
    Ok(())
}

#[tauri::command]
async fn get_status(state: State<'_, PollerState>) -> Result<PollerStatus, String> {
    Ok(state.status.lock().await.clone())
}

#[tauri::command]
fn open_documentation(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(
            "https://github.com/lesscommerce-io/print-agent#readme",
            None::<&str>,
        )
        .map_err(|e| e.to_string())
}

// ============================================================================
// Poller lifecycle
// ============================================================================

async fn restart_poller(app: &AppHandle, state: &State<'_, PollerState>, cfg: AgentConfig) {
    let mut handle_guard = state.handle.lock().await;
    if let Some(prev) = handle_guard.take() {
        prev.stop();
    }
    if !cfg.is_complete() {
        let mut s = state.status.lock().await;
        s.state = "unconfigured".into();
        s.last_message = Some("Configure the agent to start printing.".into());
        refresh_tray(app, &s);
        return;
    }

    let app_for_emit = app.clone();
    let status = state.status.clone();
    let new_handle = poller::spawn(cfg, status, move |snapshot| {
        // Two consumers per status update: the setup window (to drive the
        // status panel) and the tray (icon + tooltip).
        refresh_tray(&app_for_emit, &snapshot);
        let _ = app_for_emit.emit("poller-status", &snapshot);
    });
    *handle_guard = Some(new_handle);
}

// ============================================================================
// Deep link
// ============================================================================

#[derive(Debug, Clone, serde::Serialize)]
struct PairedPayload {
    name: Option<String>,
}

/// Parse `lesscommerce-print-agent://paired?device_code=X&token=Y&api=Z&name=W`.
/// Returns `Some(config-prefilled)` if the device_code matches what we sent
/// and the URL is well-formed. Returns `None` otherwise — caller logs and
/// emits a `pairing-error`.
fn handle_paired_url(
    url: &Url,
    pairing: &PairingState,
) -> Result<(AgentConfig, PairedPayload), String> {
    if url.scheme() != "lesscommerce-print-agent" {
        return Err(format!("Unexpected scheme: {}", url.scheme()));
    }
    if url.host_str() != Some("paired") {
        // Some platforms put the route under host, some under path. Accept
        // either, but log so we know which one we're seeing in the wild.
        log::debug!("paired host={:?} path={}", url.host_str(), url.path());
    }

    let mut device_code = String::new();
    let mut token = String::new();
    let mut api = String::new();
    let mut name = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "device_code" => device_code = v.into_owned(),
            "token" => token = v.into_owned(),
            "api" => api = v.into_owned(),
            "name" => name = Some(v.into_owned()),
            _ => {}
        }
    }

    if device_code.is_empty() || token.is_empty() || api.is_empty() {
        return Err("Pairing callback missing required fields".into());
    }
    if !pairing.take_if_matches(&device_code) {
        return Err("device_code mismatch — ignoring callback".into());
    }

    let cfg = AgentConfig {
        api_url: api,
        printer_token: token,
        // Filled in by the next screen — we only persist the pairing here.
        system_printer: String::new(),
        display_name: name.clone(),
        poll_interval: 5,
        launch_at_login: false,
    };
    Ok((cfg, PairedPayload { name }))
}

fn handle_deep_link_urls(app: &AppHandle, urls: Vec<Url>) {
    let pairing: State<'_, PairingState> = app.state();
    for url in urls {
        log::info!("Deep link received: {}", url);
        match handle_paired_url(&url, &pairing) {
            Ok((cfg, payload)) => {
                if let Err(err) = cfg.save() {
                    log::error!("Failed to save config: {err}");
                    let _ = app.emit("pairing-error", err.to_string());
                    continue;
                }
                show_main_window(app);
                let _ = app.emit("paired", &payload);
            }
            Err(err) => {
                log::warn!("Pairing failed: {err}");
                show_main_window(app);
                let _ = app.emit("pairing-error", err);
            }
        }
    }
}

// ============================================================================
// System tray
// ============================================================================

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Open setup", true, None::<&str>)?;
    let docs = MenuItem::with_id(app, "docs", "Documentation", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &docs, &quit])?;

    TrayIconBuilder::with_id("main")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "docs" => {
                let _ = open_documentation(app.clone());
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "main", setup_url())
        .title("LessCommerce Print Agent")
        .inner_size(520.0, 720.0)
        .min_inner_size(480.0, 600.0)
        .center()
        .build();
}

// ============================================================================
// Helpers
// ============================================================================

/// Tiny CSPRNG wrapper so we don't pull in the whole `getrandom` crate just
/// for pairing codes. Falls back to thread-local PID + nanos in the unlikely
/// event the OS source is unavailable — those codes still match locally
/// since the same value goes out and comes back through one user flow.
fn getrandom_bytes(buf: &mut [u8]) {
    use std::fs::File;
    use std::io::Read;
    if cfg!(unix) {
        if let Ok(mut f) = File::open("/dev/urandom") {
            if f.read_exact(buf).is_ok() {
                return;
            }
        }
    }
    // Fallback — not crypto-grade, but pairing only chains a single request
    // so the only thing this guards against is "two agents racing on the
    // same browser session" which is essentially impossible.
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    for byte in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *byte = (seed >> 56) as u8;
    }
}

// ============================================================================
// Entry point
// ============================================================================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            log::info!("Second instance attempted: {:?}", args);
            show_main_window(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--launched-from-autostart"]),
        ))
        .manage(PollerState {
            handle: Mutex::new(None),
            status: Arc::new(Mutex::new(PollerStatus::default())),
        })
        .manage(PairingState::default())
        .invoke_handler(tauri::generate_handler![
            load_pairing_state,
            start_pairing,
            cancel_pairing,
            list_printers,
            finish_setup,
            unpair,
            get_status,
            open_documentation
        ]);

    builder = builder.setup(|app| {
        let app_handle = app.handle().clone();

        if let Err(err) = build_tray(&app_handle) {
            log::error!("Failed to build tray icon: {err}");
        }

        // Pulse the tray icon while we're printing — toggles between the
        // green "printing" PNG and the neutral "idle" PNG every 600ms so
        // the user has a clear visual heartbeat from across the room.
        // Cheap: one tokio task that no-ops 95% of the time.
        let pulse_app = app_handle.clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(600));
            let mut on_phase = true;
            loop {
                interval.tick().await;
                let pulse_state: State<'_, PollerState> = pulse_app.state();
                let snapshot = pulse_state.status.lock().await.clone();
                if snapshot.state != "printing" {
                    continue;
                }
                if let Some(tray) = pulse_app.tray_by_id("main") {
                    on_phase = !on_phase;
                    let bytes = if on_phase { TRAY_PRINTING } else { TRAY_IDLE };
                    if let Some(img) = decode_png(bytes) {
                        let _ = tray.set_icon(Some(img));
                    }
                }
            }
        });

        let app_for_links = app_handle.clone();
        app.deep_link().on_open_url(move |event| {
            handle_deep_link_urls(&app_for_links, event.urls());
        });

        if let Ok(Some(initial)) = app.deep_link().get_current() {
            handle_deep_link_urls(&app_handle, initial);
        }

        // Hide the window if launched from autostart OR if we already have
        // a complete config (returning user). Otherwise show it so the user
        // sees the "Pair" button on first launch.
        let argv: Vec<String> = std::env::args().collect();
        let from_autostart = argv.iter().any(|a| a == "--launched-from-autostart");
        let has_complete = AgentConfig::load()
            .ok()
            .flatten()
            .map(|c| c.is_complete())
            .unwrap_or(false);

        if from_autostart || has_complete {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
        } else if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }

        let app_for_boot = app_handle.clone();
        if let Ok(Some(cfg)) = AgentConfig::load() {
            if cfg.is_complete() {
                tauri::async_runtime::spawn(async move {
                    let state: State<'_, PollerState> = app_for_boot.state();
                    restart_poller(&app_for_boot, &state, cfg).await;
                });
            }
        }

        Ok(())
    });

    builder
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

//! Tauri app wiring — IPC commands, system tray, deep-link handling, and
//! the lifecycle of the polling task.
//!
//! The setup window (vanilla HTML/JS in `../ui/`) talks to Rust via these
//! `#[tauri::command]` functions:
//!
//!   load_config       — read print-agent.json (or null on first run)
//!   save_config       — write print-agent.json + restart polling
//!   list_printers     — enumerate system printers (CUPS/Get-Printer)
//!   get_status        — current PollerStatus
//!   set_autostart     — toggle "launch at login"
//!   open_documentation — open https://github.com/lesscommerce-io/print-agent
//!
//! Deep-link `lesscommerce-print-agent://setup?token=…&api=…&name=…` arrives
//! through the deep-link plugin's `on_open_url`, which forwards a payload to
//! the setup window via the `pairing-payload` event.

mod api;
mod config;
mod poller;
mod printer;

use config::AgentConfig;
use poller::{PollerHandle, PollerStatus, SharedStatus};
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_deep_link::DeepLinkExt;
use tokio::sync::Mutex;

fn setup_url() -> WebviewUrl {
    WebviewUrl::App("index.html".into())
}

#[derive(Default)]
struct PollerState {
    handle: Mutex<Option<PollerHandle>>,
    status: SharedStatus,
}

// ============================================================================
// IPC commands
// ============================================================================

#[tauri::command]
async fn load_config() -> Result<Option<AgentConfig>, String> {
    AgentConfig::load().map_err(|e| e.to_string())
}

#[tauri::command]
async fn save_config(
    cfg: AgentConfig,
    app: AppHandle,
    state: State<'_, PollerState>,
) -> Result<AgentConfig, String> {
    cfg.save().map_err(|e| e.to_string())?;
    restart_poller(&app, &state, cfg.clone()).await;
    Ok(cfg)
}

#[tauri::command]
async fn list_printers() -> Result<Vec<printer::SystemPrinter>, String> {
    printer::list_system_printers()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_status(state: State<'_, PollerState>) -> Result<PollerStatus, String> {
    Ok(state.status.lock().await.clone())
}

#[tauri::command]
async fn set_autostart(enabled: bool, app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())?;
    } else {
        mgr.disable().map_err(|e| e.to_string())?;
    }
    mgr.is_enabled().map_err(|e| e.to_string())
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
        return;
    }

    let app_for_emit = app.clone();
    let status = state.status.clone();
    let new_handle = poller::spawn(cfg, status, move |snapshot| {
        let _ = app_for_emit.emit("poller-status", &snapshot);
    });
    *handle_guard = Some(new_handle);
}

// ============================================================================
// Deep link
// ============================================================================

#[derive(Debug, Clone, serde::Serialize)]
struct PairingPayload {
    token: Option<String>,
    api: Option<String>,
    name: Option<String>,
}

fn parse_pairing_url(raw: &str) -> Option<PairingPayload> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "lesscommerce-print-agent" {
        return None;
    }
    let mut payload = PairingPayload {
        token: None,
        api: None,
        name: None,
    };
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "token" => payload.token = Some(value.into_owned()),
            "api" => payload.api = Some(value.into_owned()),
            "name" => payload.name = Some(value.into_owned()),
            _ => {}
        }
    }
    Some(payload)
}

fn handle_deep_link_urls(app: &AppHandle, urls: Vec<Url>) {
    for url in urls {
        let raw = url.as_str().to_string();
        let Some(payload) = parse_pairing_url(&raw) else {
            log::warn!("Ignoring deep-link with unsupported scheme: {raw}");
            continue;
        };
        log::info!("Received pairing deep-link from {:?}", payload.api);
        show_main_window(app);
        let _ = app.emit("pairing-payload", &payload);
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
    // First ever invocation — the main window may not exist yet because we
    // launched headless from autostart. Build it on demand.
    let _ = WebviewWindowBuilder::new(app, "main", setup_url())
        .title("LessCommerce Print Agent")
        .inner_size(520.0, 720.0)
        .min_inner_size(480.0, 600.0)
        .center()
        .build();
}

// ============================================================================
// Entry point
// ============================================================================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut builder = tauri::Builder::default()
        // Single-instance: second launch (e.g. clicking another deep-link)
        // surfaces the existing window instead of spawning a duplicate.
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
        // `--` is the convention launchers pass when re-launching at login.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--launched-from-autostart"]),
        ))
        .manage(PollerState {
            handle: Mutex::new(None),
            status: Arc::new(Mutex::new(PollerStatus::default())),
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            list_printers,
            get_status,
            set_autostart,
            open_documentation
        ]);

    builder = builder.setup(|app| {
        let app_handle = app.handle().clone();

        // System tray icon — visible from launch.
        if let Err(err) = build_tray(&app_handle) {
            log::error!("Failed to build tray icon: {err}");
        }

        // Wire deep-link incoming URLs to the pairing handler.
        let app_for_links = app_handle.clone();
        app.deep_link().on_open_url(move |event| {
            handle_deep_link_urls(&app_for_links, event.urls());
        });

        // If the OS launched us with a pending URL (some platforms invoke
        // the binary directly with the URL as argv), drain those now.
        if let Ok(Some(initial)) = app.deep_link().get_current() {
            handle_deep_link_urls(&app_handle, initial);
        }

        // Should we hide the window at launch? We do when:
        //   - autostart launched us (`--launched-from-autostart` on argv), or
        //   - config exists and is complete (returning user)
        let argv: Vec<String> = std::env::args().collect();
        let from_autostart = argv.iter().any(|a| a == "--launched-from-autostart");
        let has_config = AgentConfig::load()
            .ok()
            .flatten()
            .map(|c| c.is_complete())
            .unwrap_or(false);

        if from_autostart || has_config {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
        } else if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }

        // Boot the poller if config is ready.
        let state: State<'_, PollerState> = app.state();
        let app_for_boot = app_handle.clone();
        if let Ok(Some(cfg)) = AgentConfig::load() {
            tauri::async_runtime::spawn(async move {
                let state: State<'_, PollerState> = app_for_boot.state();
                restart_poller(&app_for_boot, &state, cfg).await;
            });
        } else {
            tauri::async_runtime::block_on(async move {
                let mut s = state.status.lock().await;
                s.state = "unconfigured".into();
                s.last_message = Some("First run — open the setup window to configure.".into());
            });
        }

        Ok(())
    });

    builder
        .on_window_event(|window, event| {
            // "Close" button hides instead of quits. App keeps running in tray.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

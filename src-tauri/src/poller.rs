//! The actual heartbeat → fetch → print → ack loop.
//!
//! Runs as a long-lived tokio task spawned at app start (after config is
//! loaded). Stops when the `stop` watch flips to `true` — that happens on
//! "Quit" from the system tray or on a config reload.

use crate::api::{HeartbeatData, PrintApi};
use crate::config::AgentConfig;
use crate::printer::print_label;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::time::sleep;

/// Live status the system tray + setup window listen to. Emitted via a Tauri
/// event whenever it changes.
#[derive(Debug, Clone, Serialize)]
pub struct PollerStatus {
    pub state: String, // idle | polling | printing | error | unconfigured | stopped
    pub last_message: Option<String>,
    pub last_printed_at: Option<String>,
    pub last_error_at: Option<String>,
    pub jobs_printed: u64,
    pub jobs_failed: u64,
    pub printer_name: Option<String>, // friendly name returned by heartbeat
}

impl Default for PollerStatus {
    fn default() -> Self {
        Self {
            state: "unconfigured".into(),
            last_message: None,
            last_printed_at: None,
            last_error_at: None,
            jobs_printed: 0,
            jobs_failed: 0,
            printer_name: None,
        }
    }
}

pub type SharedStatus = Arc<Mutex<PollerStatus>>;

#[derive(Clone)]
pub struct PollerHandle {
    stop_tx: watch::Sender<bool>,
}

impl PollerHandle {
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }
}

/// Spawns the polling task. Caller keeps the returned handle so it can stop
/// the task on app exit / config reload. The `on_status` closure is invoked
/// every time the status changes — the caller wires it to a Tauri event.
pub fn spawn<F>(config: AgentConfig, status: SharedStatus, on_status: F) -> PollerHandle
where
    F: Fn(PollerStatus) + Send + Sync + 'static,
{
    let (stop_tx, mut stop_rx) = watch::channel(false);

    let handle = PollerHandle { stop_tx };

    tokio::spawn(async move {
        let api = match PrintApi::new(&config.api_url, &config.printer_token) {
            Ok(api) => api,
            Err(err) => {
                update_status(&status, &on_status, |s| {
                    s.state = "error".into();
                    s.last_message = Some(format!("Bad API URL or token: {err}"));
                    s.last_error_at = Some(now_iso());
                })
                .await;
                return;
            }
        };
        let host = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let mut interval_secs = config.poll_interval.max(1);

        loop {
            if *stop_rx.borrow() {
                update_status(&status, &on_status, |s| {
                    s.state = "stopped".into();
                })
                .await;
                return;
            }

            // Heartbeat. Server may push a different poll cadence and a
            // friendly name we surface in the tray tooltip.
            match api.heartbeat(&config.system_printer, &host).await {
                Ok(HeartbeatData {
                    name,
                    poll_interval_seconds,
                    ..
                }) => {
                    interval_secs = poll_interval_seconds.max(1);
                    update_status(&status, &on_status, |s| {
                        s.printer_name = Some(name.clone());
                        if s.state == "error" || s.state == "unconfigured" {
                            s.state = "polling".into();
                            s.last_message = Some("Connected".into());
                        }
                    })
                    .await;
                }
                Err(err) => {
                    log::warn!("heartbeat failed: {err}");
                    update_status(&status, &on_status, |s| {
                        s.state = "error".into();
                        s.last_message = Some(format!("Heartbeat: {err}"));
                        s.last_error_at = Some(now_iso());
                    })
                    .await;
                    if !sleep_with_stop(&mut stop_rx, interval_secs.max(10)).await {
                        return;
                    }
                    continue;
                }
            }

            // Fetch + print loop — process bursts back-to-back without
            // sleeping between, so 10 queued labels print as fast as the
            // printer chews through them.
            loop {
                if *stop_rx.borrow() {
                    return;
                }
                let job = match api.fetch_next().await {
                    Ok(Some(job)) => job,
                    Ok(None) => break, // queue empty → drop into outer sleep
                    Err(err) => {
                        log::warn!("fetch_next failed: {err}");
                        update_status(&status, &on_status, |s| {
                            s.state = "error".into();
                            s.last_message = Some(format!("Fetch: {err}"));
                            s.last_error_at = Some(now_iso());
                        })
                        .await;
                        break;
                    }
                };

                let job_short = job.uuid.chars().take(8).collect::<String>();
                update_status(&status, &on_status, |s| {
                    s.state = "printing".into();
                    s.last_message = Some(format!(
                        "Printing {job_short}… (tracking {tracking})",
                        tracking = if job.tracking.is_empty() {
                            "—"
                        } else {
                            &job.tracking
                        }
                    ));
                })
                .await;

                match print_label(&job.bytes, &job.format, &config.system_printer).await {
                    Ok(duration_ms) => {
                        if let Err(err) = api.ack_printed(&job.uuid, duration_ms).await {
                            log::warn!("ack_printed failed: {err}");
                        }
                        update_status(&status, &on_status, |s| {
                            s.state = "polling".into();
                            s.jobs_printed += 1;
                            s.last_printed_at = Some(now_iso());
                            s.last_message =
                                Some(format!("Printed {job_short} in {duration_ms}ms"));
                        })
                        .await;
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        log::warn!("print_label failed: {msg}");
                        if let Err(ack_err) = api.ack_failed(&job.uuid, &msg).await {
                            log::warn!("ack_failed also failed: {ack_err}");
                        }
                        update_status(&status, &on_status, |s| {
                            s.state = "error".into();
                            s.jobs_failed += 1;
                            s.last_error_at = Some(now_iso());
                            s.last_message = Some(format!("Print failed: {msg}"));
                        })
                        .await;
                    }
                }
            }

            update_status(&status, &on_status, |s| {
                if s.state == "printing" {
                    s.state = "polling".into();
                }
                if s.state != "error" {
                    s.state = "idle".into();
                }
            })
            .await;

            if !sleep_with_stop(&mut stop_rx, interval_secs).await {
                return;
            }
        }
    });

    handle
}

async fn sleep_with_stop(stop_rx: &mut watch::Receiver<bool>, secs: u64) -> bool {
    tokio::select! {
        _ = sleep(Duration::from_secs(secs)) => true,
        _ = stop_rx.changed() => !*stop_rx.borrow(),
    }
}

async fn update_status<F, G>(status: &SharedStatus, on_status: &G, mutate: F)
where
    F: FnOnce(&mut PollerStatus),
    G: Fn(PollerStatus),
{
    let snapshot = {
        let mut guard = status.lock().await;
        mutate(&mut guard);
        guard.clone()
    };
    on_status(snapshot);
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // Cheap RFC3339-ish "secs.millisZ" — we only display in the UI, no need
    // for a whole `chrono` dep.
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    format!("{}.{:03}Z", secs, millis)
}

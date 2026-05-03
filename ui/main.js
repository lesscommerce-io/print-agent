// Setup window logic. Talks to Rust via Tauri's `invoke()`. Listens for two
// events:
//   poller-status   — emitted by the polling task whenever its state changes
//   pairing-payload — emitted when a deep link `lesscommerce-print-agent://`
//                     arrives; we pre-fill the form so the user only clicks
//                     "Save and run".

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

// =============================================================================
// DOM refs
// =============================================================================

const $ = (id) => document.getElementById(id);
const els = {
  versionBadge: $("version"),
  statusPanel: $("status-panel"),
  statusDot: $("status-dot"),
  statusState: $("status-state"),
  statusMessage: $("status-message"),
  statusPrinter: $("status-printer"),
  statusPrinted: $("status-printed"),
  statusFailed: $("status-failed"),
  statusLast: $("status-last"),

  form: $("setup-form"),
  apiUrl: $("api-url"),
  printerToken: $("printer-token"),
  systemPrinter: $("system-printer"),
  launchAtLogin: $("launch-at-login"),
  refreshPrinters: $("refresh-printers"),
  saveBtn: $("save-btn"),
  formMessage: $("form-message"),

  openDocs: $("open-docs"),
  formHeading: $("form-heading"),
  formSubtitle: $("form-subtitle"),
};

const stateLabels = {
  idle: "Czekam na zlecenia",
  polling: "Łączenie z serwerem…",
  printing: "Drukuję…",
  error: "Błąd",
  unconfigured: "Nieskonfigurowane",
  stopped: "Zatrzymane",
};

// =============================================================================
// Init
// =============================================================================

async function init() {
  hydrateConfigPath();

  // Load saved config (if any) and pre-fill the form.
  let cfg = null;
  try {
    cfg = await invoke("load_config");
  } catch (err) {
    console.error("load_config failed", err);
  }
  if (cfg) applyConfigToForm(cfg);

  // Refresh the system-printer dropdown.
  await refreshPrinters(cfg?.system_printer);

  // Show status panel if we have a complete config — otherwise it's the first
  // run and the form is the focus.
  if (cfg && cfg.api_url && cfg.printer_token && cfg.system_printer) {
    els.statusPanel.classList.remove("hidden");
    els.formHeading.textContent = "Aktualizuj konfigurację";
    els.formSubtitle.textContent =
      "Edytuj poniżej i kliknij „Zapisz" — agent przeładuje połączenie.";
    refreshStatus();
  }

  // Push live status updates from Rust.
  listen("poller-status", (event) => {
    renderStatus(event.payload);
    els.statusPanel.classList.remove("hidden");
  });

  // Pre-fill from deep-link payload (overrides any existing form values —
  // explicit user intent beats whatever's typed).
  listen("pairing-payload", (event) => {
    const payload = event.payload || {};
    if (payload.api) els.apiUrl.value = payload.api;
    if (payload.token) els.printerToken.value = payload.token;
    flashMessage(`Wczytano dane z panelu: ${payload.name || "drukarka"}`);
    // Bring the window forward (deep link triggers from background).
    try { getCurrentWindow().setFocus(); } catch {}
  });

  // Wire up the form.
  els.form.addEventListener("submit", onSubmit);
  els.refreshPrinters.addEventListener("click", () => refreshPrinters(els.systemPrinter.value));
  els.openDocs.addEventListener("click", () => invoke("open_documentation").catch(console.error));
}

function hydrateConfigPath() {
  // Best-effort placeholder — real path lives Rust-side. We just set a hint
  // for the user; the exact directory differs per OS.
  const ua = navigator.userAgent.toLowerCase();
  let path = "~/.config/lesscommerce/print-agent.json";
  if (ua.includes("windows") || ua.includes("win64")) {
    path = "%APPDATA%\\lesscommerce\\print-agent.json";
  } else if (ua.includes("mac os")) {
    path = "~/Library/Application Support/io.lesscommerce.print-agent/print-agent.json";
  }
  $("config-path").textContent = path;
}

function applyConfigToForm(cfg) {
  els.apiUrl.value = cfg.api_url || "";
  els.printerToken.value = cfg.printer_token || "";
  els.launchAtLogin.checked = !!cfg.launch_at_login;
  // The select is populated async — we'll set the value after refresh.
  els.systemPrinter.dataset.preselect = cfg.system_printer || "";
}

async function refreshPrinters(preferred) {
  const select = els.systemPrinter;
  select.innerHTML = '<option value="">— ładowanie… —</option>';
  try {
    const list = await invoke("list_printers");
    select.innerHTML = '<option value="">— wybierz drukarkę —</option>';
    for (const p of list) {
      const opt = document.createElement("option");
      opt.value = p.name;
      opt.textContent = p.name;
      select.appendChild(opt);
    }
    const target = preferred || select.dataset.preselect;
    if (target) select.value = target;
    if (list.length === 0) {
      $("printers-help").textContent =
        "Nie wykryto żadnych drukarek systemowych. Zainstaluj sterownik drukarki w systemie i kliknij ↻.";
    } else {
      $("printers-help").textContent = `Wykryto ${list.length} drukarek w systemie.`;
    }
  } catch (err) {
    select.innerHTML = '<option value="">— błąd: ' + (err || "unknown") + " —</option>";
    $("printers-help").textContent = "Nie udało się pobrać listy. Czy CUPS / Spooler działa?";
  }
}

async function onSubmit(event) {
  event.preventDefault();
  els.saveBtn.disabled = true;
  flashMessage("Zapisuję…");

  const cfg = {
    api_url: els.apiUrl.value.trim().replace(/\/+$/, ""),
    printer_token: els.printerToken.value.trim(),
    system_printer: els.systemPrinter.value.trim(),
    launch_at_login: els.launchAtLogin.checked,
    poll_interval: 5,
    display_name: null,
  };

  try {
    await invoke("save_config", { cfg });
    // Toggle autostart per user choice.
    try {
      await invoke("set_autostart", { enabled: cfg.launch_at_login });
    } catch (err) {
      console.warn("autostart toggle failed", err);
    }
    flashMessage("Zapisane. Agent się łączy…", "ok");
    els.statusPanel.classList.remove("hidden");
    refreshStatus();
  } catch (err) {
    console.error("save_config failed", err);
    flashMessage(typeof err === "string" ? err : "Błąd zapisu", "error");
  } finally {
    els.saveBtn.disabled = false;
  }
}

async function refreshStatus() {
  try {
    const status = await invoke("get_status");
    renderStatus(status);
  } catch (err) {
    console.warn("get_status failed", err);
  }
}

function renderStatus(status) {
  els.statusDot.className = "status-dot " + (status.state || "unconfigured");
  els.statusState.textContent = stateLabels[status.state] || status.state || "—";
  els.statusMessage.textContent = status.last_message || "";
  els.statusPrinter.textContent = status.printer_name || "—";
  els.statusPrinted.textContent = status.jobs_printed ?? 0;
  els.statusFailed.textContent = status.jobs_failed ?? 0;
  els.statusLast.textContent = formatRelative(status.last_printed_at);
}

function formatRelative(iso) {
  if (!iso) return "—";
  // Our Rust uses "<unix-secs>.<ms>Z". Parse cheaply.
  const match = /^(\d+)\.(\d+)Z$/.exec(iso);
  const date = match ? new Date(parseInt(match[1], 10) * 1000) : new Date(iso);
  if (isNaN(date.getTime())) return "—";
  return date.toLocaleTimeString();
}

function flashMessage(text, kind) {
  els.formMessage.textContent = text || "";
  els.formMessage.style.color = kind === "ok" ? "var(--success)"
    : kind === "error" ? "var(--error)"
    : "var(--muted)";
}

init();

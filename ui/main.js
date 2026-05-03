// Setup window controller. Three screens:
//   1. PAIR     — single button "Powiąż z LessCommerce" → opens panel page
//                 in default browser, waits for the deep-link to come back
//   2. PRINTER  — pick the system printer (only thing that varies per host)
//   3. STATUS   — live "currently printing X" feedback
//
// Rust backend exposes:
//   start_pairing()         → opens browser, returns the device_code we expect
//   load_pairing_state()    → has the user finished pairing? returns config or null
//   list_printers()
//   finish_setup({ system_printer, launch_at_login })  → saves config + starts polling
//   unpair()                → wipes config, returns to PAIR screen
//   get_status()
//   open_documentation()

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

const $ = (id) => document.getElementById(id);
const SCREENS = ["pair", "printer", "status"];

const stateLabels = {
  idle: "Czekam na zlecenia",
  polling: "Łączenie z serwerem…",
  printing: "Drukuję…",
  error: "Błąd",
  unconfigured: "Nieskonfigurowane",
  stopped: "Zatrzymane",
};

// =============================================================================
// Screen routing
// =============================================================================

function show(screen) {
  for (const id of SCREENS) {
    $(`screen-${id}`).classList.toggle("hidden", id !== screen);
  }
}

// =============================================================================
// Screen 1: PAIR
// =============================================================================

let pairingActive = false;

async function startPairing() {
  if (pairingActive) return;
  pairingActive = true;
  $("btn-start-pair").disabled = true;
  $("pair-status").classList.remove("hidden");
  try {
    await invoke("start_pairing");
    // The deep-link listener (below) will flip us to the printer screen
    // once the browser bounces back. Nothing else to do here.
  } catch (err) {
    console.error("start_pairing failed", err);
    $("btn-start-pair").disabled = false;
    pairingActive = false;
    $("pair-status").classList.add("hidden");
    alert("Nie udało się otworzyć przeglądarki: " + err);
  }
}

function cancelPairing() {
  pairingActive = false;
  $("btn-start-pair").disabled = false;
  $("pair-status").classList.add("hidden");
  invoke("cancel_pairing").catch(() => {});
}

// =============================================================================
// Screen 2: PRINTER PICK
// =============================================================================

async function refreshPrinters(preferred) {
  const select = $("system-printer");
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
    if (preferred) select.value = preferred;
    $("printers-help").textContent = list.length
      ? `Wykryto ${list.length} drukarek w systemie.`
      : "Nie wykryto żadnych drukarek systemowych. Zainstaluj sterownik drukarki w systemie i kliknij ↻.";
  } catch (err) {
    select.innerHTML = `<option value="">— błąd: ${err} —</option>`;
    $("printers-help").textContent = "Nie udało się pobrać listy. Czy CUPS / Spooler działa?";
  }
}

async function finishSetup() {
  const systemPrinter = $("system-printer").value.trim();
  if (!systemPrinter) {
    $("finish-message").textContent = "Wybierz drukarkę.";
    $("finish-message").style.color = "var(--error)";
    return;
  }
  const launchAtLogin = $("launch-at-login").checked;

  $("btn-finish").disabled = true;
  $("finish-message").textContent = "Zapisuję…";
  $("finish-message").style.color = "var(--muted)";
  try {
    await invoke("finish_setup", { systemPrinter, launchAtLogin });
    show("status");
    refreshStatus();
  } catch (err) {
    console.error("finish_setup failed", err);
    $("finish-message").textContent = String(err);
    $("finish-message").style.color = "var(--error)";
    $("btn-finish").disabled = false;
  }
}

// =============================================================================
// Screen 3: STATUS
// =============================================================================

async function refreshStatus() {
  try {
    const cfg = await invoke("load_pairing_state");
    if (cfg) {
      $("status-system-printer").textContent = cfg.system_printer || "—";
    }
    const status = await invoke("get_status");
    renderStatus(status);
  } catch (err) {
    console.warn("get_status failed", err);
  }
}

function renderStatus(status) {
  $("status-dot").className = "status-dot " + (status.state || "unconfigured");
  $("status-state").textContent = stateLabels[status.state] || status.state || "—";
  $("status-message").textContent = status.last_message || "";
  $("status-printer").textContent = status.printer_name || "—";
  $("status-printed").textContent = status.jobs_printed ?? 0;
  $("status-failed").textContent = status.jobs_failed ?? 0;
}

async function unpair() {
  if (!confirm("Na pewno odpiąć tę drukarkę? Przed kolejnym drukowaniem trzeba sparować ponownie.")) {
    return;
  }
  try {
    await invoke("unpair");
    show("pair");
    pairingActive = false;
    $("btn-start-pair").disabled = false;
    $("pair-status").classList.add("hidden");
  } catch (err) {
    alert("Nie udało się odpiąć: " + err);
  }
}

// =============================================================================
// Boot
// =============================================================================

async function init() {
  // Decide initial screen: if we already have a complete config, jump
  // straight to status. Otherwise show the Pair button.
  let cfg = null;
  try {
    cfg = await invoke("load_pairing_state");
  } catch (err) {
    console.error("load_pairing_state failed", err);
  }

  if (cfg && cfg.system_printer) {
    show("status");
    refreshStatus();
  } else if (cfg) {
    // Mid-flow: pairing finished but printer not yet picked. This happens
    // if the user closed the window between deep-link return and finish.
    show("printer");
    $("paired-name").textContent = cfg.display_name || "drukarką";
    refreshPrinters(cfg.system_printer);
  } else {
    show("pair");
  }

  // The Rust side fires this event the moment the deep-link arrives —
  // we flip to the printer screen with the paired display name.
  listen("paired", (event) => {
    const payload = event.payload || {};
    pairingActive = false;
    $("btn-start-pair").disabled = false;
    $("pair-status").classList.add("hidden");
    show("printer");
    $("paired-name").textContent = payload.name || "drukarką";
    refreshPrinters();
    try { getCurrentWindow().setFocus(); } catch {}
  });

  listen("pairing-error", (event) => {
    pairingActive = false;
    $("btn-start-pair").disabled = false;
    $("pair-status").classList.add("hidden");
    alert("Powiązanie nie powiodło się: " + (event.payload || ""));
  });

  listen("poller-status", (event) => {
    if (!$("screen-status").classList.contains("hidden")) {
      renderStatus(event.payload);
    }
  });

  // Wire buttons
  $("btn-start-pair").addEventListener("click", startPairing);
  $("btn-cancel-pair").addEventListener("click", cancelPairing);
  $("refresh-printers").addEventListener("click", () => refreshPrinters($("system-printer").value));
  $("btn-finish").addEventListener("click", finishSetup);
  $("btn-unpair").addEventListener("click", unpair);
  $("open-docs").addEventListener("click", () => invoke("open_documentation").catch(console.error));
}

init();

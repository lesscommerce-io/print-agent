// Setup window controller. Three screens:
//   1. PAIR     — "Powiąż z LessCommerce" → opens panel page in the default
//                 browser, waits for the deep-link to come back
//   2. PRINTER  — pick the system printer for the freshly paired entry
//                 (or change it for an existing one)
//   3. STATUS   — list of printers, each with live state + counters
//
// One agent can serve several printers: every "Dodaj drukarkę" is a new
// pairing (a separate printer in the panel, its own token) and its own
// polling loop on the Rust side.
//
// Rust backend exposes:
//   start_pairing() / cancel_pairing()
//   load_pairing_state()    → { printers: [...], pending, launch_at_login } | null
//   list_printers()
//   finish_setup({ systemPrinter, launchAtLogin })   → assigns printer to the pending entry
//   set_system_printer({ id, systemPrinter })        → changes it for an existing one
//   remove_printer({ id })  /  unpair()
//   get_status()            → [{ id, state, printer_name, system_printer, ... }]
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

let config = null; // last load_pairing_state()
let editingId = null; // set when changing the system printer of an existing entry

// =============================================================================
// Screen routing
// =============================================================================

function show(screen) {
  for (const id of SCREENS) {
    $(`screen-${id}`).classList.toggle("hidden", id !== screen);
  }
}

async function reloadConfig() {
  try {
    config = await invoke("load_pairing_state");
  } catch (err) {
    console.error("load_pairing_state failed", err);
    config = null;
  }
  return config;
}

const hasPrinters = () => !!(config && config.printers && config.printers.length);

// =============================================================================
// Screen 1: PAIR
// =============================================================================

let pairingActive = false;

function showPair() {
  show("pair");
  $("btn-back-list").classList.toggle("hidden", !hasPrinters());
}

async function startPairing() {
  if (pairingActive) return;
  pairingActive = true;
  $("btn-start-pair").disabled = true;
  $("pair-status").classList.remove("hidden");
  try {
    await invoke("start_pairing");
    // The deep-link listener (below) flips us to the printer screen once the
    // browser bounces back.
  } catch (err) {
    console.error("start_pairing failed", err);
    resetPairButton();
    alert("Nie udało się otworzyć przeglądarki: " + err);
  }
}

function resetPairButton() {
  pairingActive = false;
  $("btn-start-pair").disabled = false;
  $("pair-status").classList.add("hidden");
}

function cancelPairing() {
  resetPairButton();
  invoke("cancel_pairing").catch(() => {});
}

// =============================================================================
// Screen 2: PRINTER PICK
// =============================================================================

function showPrinterPick({ name, preferred, edit }) {
  editingId = edit || null;
  show("printer");
  $("printer-title").textContent = edit ? "Zmień drukarkę systemową" : "Ostatni krok";
  $("paired-name").textContent = name || "drukarką";
  $("launch-row").classList.toggle("hidden", !!edit || hasPrinters());
  $("btn-finish").textContent = edit ? "Zapisz" : "Zacznij drukować";
  $("btn-finish").disabled = false;
  $("finish-message").textContent = "";
  refreshPrinters(preferred);
}

async function refreshPrinters(preferred) {
  const select = $("system-printer");
  select.innerHTML = '<option value="">— ładowanie… —</option>';
  try {
    const list = await invoke("list_printers");
    select.innerHTML = '<option value="">— wybierz drukarkę —</option>';
    const used = new Set((config?.printers || []).filter((p) => p.id !== editingId).map((p) => p.system_printer));
    for (const p of list) {
      const opt = document.createElement("option");
      opt.value = p.name;
      opt.textContent = used.has(p.name) ? `${p.name} (już używana)` : p.name;
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

  $("btn-finish").disabled = true;
  $("finish-message").textContent = "Zapisuję…";
  $("finish-message").style.color = "var(--muted)";
  try {
    if (editingId) {
      await invoke("set_system_printer", { id: editingId, systemPrinter });
    } else {
      const launchAtLogin = hasPrinters() ? !!config.launch_at_login : $("launch-at-login").checked;
      await invoke("finish_setup", { systemPrinter, launchAtLogin });
    }
    editingId = null;
    await showStatus();
  } catch (err) {
    console.error("finish_setup failed", err);
    $("finish-message").textContent = String(err);
    $("finish-message").style.color = "var(--error)";
    $("btn-finish").disabled = false;
  }
}

async function cancelPrinterPick() {
  if (!editingId && config?.pending) {
    // Porzucone parowanie — nie zostawiamy martwego wpisu.
    await invoke("remove_printer", { id: config.pending.id }).catch(() => {});
  }
  editingId = null;
  await reloadConfig();
  if (hasPrinters()) {
    await showStatus();
  } else {
    showPair();
  }
}

// =============================================================================
// Screen 3: STATUS (list of printers)
// =============================================================================

async function showStatus() {
  await reloadConfig();
  show("status");
  try {
    renderList(await invoke("get_status"));
  } catch (err) {
    console.warn("get_status failed", err);
    renderList([]);
  }
}

function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function renderList(statuses) {
  const box = $("printer-list");
  box.innerHTML = "";
  const byId = Object.fromEntries((statuses || []).map((s) => [s.id, s]));
  const printers = config?.printers || [];

  if (!printers.length) {
    box.appendChild(el("div", "printer-empty", "Brak drukarek. Kliknij „Dodaj drukarkę”."));
    return;
  }

  for (const p of printers) {
    const st = byId[p.id] || { state: "polling" };
    const item = el("div", "printer-item");

    const row = el("div", "status-row");
    row.appendChild(el("div", "status-dot " + (st.state || "unconfigured")));
    const text = el("div", "status-text");
    text.appendChild(el("div", "status-state", st.printer_name || p.display_name || "Drukarka"));
    text.appendChild(el("div", "status-message muted", `${stateLabels[st.state] || st.state || "—"}${st.last_message ? " · " + st.last_message : ""}`));
    row.appendChild(text);
    item.appendChild(row);

    const meta = el("div", "meta");
    meta.appendChild(el("span", "", `Drukarka systemowa: ${p.system_printer || "—"}`));
    meta.appendChild(el("span", "", `Wydrukowane: ${st.jobs_printed ?? 0}`));
    meta.appendChild(el("span", "", `Błędy: ${st.jobs_failed ?? 0}`));
    item.appendChild(meta);

    const actions = el("div", "actions");
    const change = el("button", "btn btn-ghost", "Zmień drukarkę systemową");
    change.type = "button";
    change.addEventListener("click", () =>
      showPrinterPick({ name: p.display_name, preferred: p.system_printer, edit: p.id })
    );
    const remove = el("button", "btn btn-ghost", "Odepnij");
    remove.type = "button";
    remove.addEventListener("click", () => removePrinter(p));
    actions.appendChild(change);
    actions.appendChild(remove);
    item.appendChild(actions);

    box.appendChild(item);
  }
}

async function removePrinter(p) {
  if (!confirm(`Odpiąć „${p.display_name || p.system_printer}”? Etykiety przestaną na nią trafiać.`)) {
    return;
  }
  try {
    await invoke("remove_printer", { id: p.id });
    await reloadConfig();
    if (hasPrinters()) {
      await showStatus();
    } else {
      showPair();
    }
  } catch (err) {
    alert("Nie udało się odpiąć: " + err);
  }
}

async function unpairAll() {
  if (!confirm("Odpiąć wszystkie drukarki? Przed kolejnym drukowaniem trzeba sparować ponownie.")) {
    return;
  }
  try {
    await invoke("unpair");
    config = null;
    resetPairButton();
    showPair();
  } catch (err) {
    alert("Nie udało się odpiąć: " + err);
  }
}

// =============================================================================
// Boot
// =============================================================================

async function init() {
  await reloadConfig();

  if (config?.pending) {
    // Mid-flow: pairing finished but printer not yet picked.
    showPrinterPick({ name: config.pending.display_name });
  } else if (hasPrinters()) {
    await showStatus();
  } else {
    showPair();
  }

  listen("paired", async (event) => {
    const payload = event.payload || {};
    resetPairButton();
    await reloadConfig();
    showPrinterPick({ name: payload.name });
    try { getCurrentWindow().setFocus(); } catch {}
  });

  listen("pairing-error", (event) => {
    resetPairButton();
    alert("Powiązanie nie powiodło się: " + (event.payload || ""));
  });

  listen("poller-status", (event) => {
    if (!$("screen-status").classList.contains("hidden")) {
      renderList(event.payload);
    }
  });

  $("btn-start-pair").addEventListener("click", startPairing);
  $("btn-cancel-pair").addEventListener("click", cancelPairing);
  $("btn-back-list").addEventListener("click", () => { cancelPairing(); showStatus(); });
  $("refresh-printers").addEventListener("click", () => refreshPrinters($("system-printer").value));
  $("btn-finish").addEventListener("click", finishSetup);
  $("btn-cancel-printer").addEventListener("click", cancelPrinterPick);
  $("btn-add-printer").addEventListener("click", showPair);
  $("btn-unpair").addEventListener("click", unpairAll);
  $("open-docs").addEventListener("click", () => invoke("open_documentation").catch(console.error));
}

init();

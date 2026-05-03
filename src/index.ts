#!/usr/bin/env bun
/**
 * LessCommerce Print Agent
 *
 * A small daemon that runs on the merchant's local computer (the one with
 * the label printer plugged into it) and polls the LessCommerce API for
 * pending print jobs. When it finds one, it streams the label PDF/ZPL to
 * the system printer and acks the job.
 *
 * Loop:
 *   1. heartbeat — bumps last_seen_at server-side, learns poll interval
 *   2. GET /api/print/jobs/next  → 204 (idle) or 200 + bytes + headers
 *   3. send-to-printer (SumatraPDF on Win, lp on Mac/Linux)
 *   4. POST /api/print/jobs/{uuid}/ack
 *
 * The bearer token authenticates the agent. Configuration lives in a JSON
 * file at the OS-default config path; setup is interactive.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir, hostname, platform } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";

const VERSION = "0.1.0";
const USER_AGENT = `lesscommerce-print-agent/${VERSION}`;

interface AgentConfig {
  api_url: string;
  printer_token: string;
  system_printer: string;
  poll_interval: number; // seconds
}

// =============================================================================
// Config file IO
// =============================================================================

function configPath(): string {
  // ~/.config/lesscommerce/print-agent.json on Linux/Mac
  // %APPDATA%\lesscommerce\print-agent.json on Windows
  const home = homedir();
  if (platform() === "win32") {
    const appData = process.env.APPDATA || join(home, "AppData", "Roaming");
    return join(appData, "lesscommerce", "print-agent.json");
  }
  return join(home, ".config", "lesscommerce", "print-agent.json");
}

function loadConfig(): AgentConfig | null {
  const path = configPath();
  if (!existsSync(path)) return null;
  try {
    const data = JSON.parse(readFileSync(path, "utf8"));
    return data as AgentConfig;
  } catch (err) {
    console.error(`[config] Failed to read ${path}: ${(err as Error).message}`);
    return null;
  }
}

function saveConfig(config: AgentConfig): void {
  const path = configPath();
  const dir = path.substring(0, path.lastIndexOf(platform() === "win32" ? "\\" : "/"));
  mkdirSync(dir, { recursive: true });
  writeFileSync(path, JSON.stringify(config, null, 2), "utf8");
}

// =============================================================================
// System printer discovery
// =============================================================================

async function listSystemPrinters(): Promise<string[]> {
  if (platform() === "win32") {
    return runAndCollect("powershell", [
      "-NoProfile",
      "-Command",
      "Get-Printer | Select-Object -ExpandProperty Name",
    ]);
  }
  // CUPS — present on Linux and macOS
  return runAndCollect("lpstat", ["-p"]).then((lines) =>
    lines
      .map((l) => l.match(/^printer\s+(\S+)/))
      .filter((m): m is RegExpMatchArray => m !== null)
      .map((m) => m[1] as string)
  );
}

function runAndCollect(cmd: string, args: string[]): Promise<string[]> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    const child = spawn(cmd, args);
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0) {
        reject(new Error(`${cmd} exited ${code}`));
        return;
      }
      resolve(
        stdout
          .split(/\r?\n/)
          .map((l) => l.trim())
          .filter((l) => l.length > 0)
      );
    });
  });
}

// =============================================================================
// Print drivers
// =============================================================================

/**
 * Send the label bytes to the configured system printer. Format dictates the
 * driver:
 *   - PDF → SumatraPDF on Windows (`-print-to`), `lp` on Mac/Linux
 *   - ZPL/EPL → raw bytes via `lp -o raw` (CUPS) or PowerShell raw print
 *
 * On any error throws — the caller acks the job as failed with the message.
 */
async function printLabel(bytes: Buffer, format: string, systemPrinter: string): Promise<void> {
  const tmpFile = join(
    process.env.TMPDIR || process.env.TEMP || "/tmp",
    `lpa-${Date.now()}-${Math.random().toString(36).slice(2, 8)}.${format === "pdf" ? "pdf" : "prn"}`
  );
  writeFileSync(tmpFile, bytes);

  try {
    if (platform() === "win32") {
      await printOnWindows(tmpFile, format, systemPrinter);
    } else {
      await printOnUnix(tmpFile, format, systemPrinter);
    }
  } finally {
    try {
      // Best-effort cleanup; some Windows print spoolers hold the file briefly.
      // Worth retrying once after a short delay.
      await new Promise((r) => setTimeout(r, 500));
      const { unlinkSync } = await import("node:fs");
      unlinkSync(tmpFile);
    } catch {
      // Ignore — the OS will clean up tmp eventually.
    }
  }
}

async function printOnWindows(file: string, format: string, printer: string): Promise<void> {
  if (format === "pdf") {
    // SumatraPDF — bundled or expected on PATH. Silent, deterministic, free.
    // Fallback to `Start-Process -Verb Print` if SumatraPDF is missing.
    const sumatra = process.env.SUMATRA_PDF || "SumatraPDF.exe";
    await runOrThrow(sumatra, ["-print-to", printer, "-silent", file], {
      onMissing: () => printOnWindowsFallback(file, printer),
    });
    return;
  }
  // ZPL/EPL — raw to default printer port.
  await runOrThrow("powershell", [
    "-NoProfile",
    "-Command",
    `Get-Content -Encoding Byte -Path '${file}' | Out-Printer -Name '${printer}'`,
  ]);
}

async function printOnWindowsFallback(file: string, printer: string): Promise<void> {
  await runOrThrow("powershell", [
    "-NoProfile",
    "-Command",
    `Start-Process -FilePath '${file}' -Verb PrintTo -ArgumentList '"${printer}"' -WindowStyle Hidden -Wait`,
  ]);
}

async function printOnUnix(file: string, format: string, printer: string): Promise<void> {
  const args = ["-d", printer];
  if (format === "zpl" || format === "epl") {
    args.push("-o", "raw");
  }
  args.push(file);
  await runOrThrow("lp", args);
}

interface RunOptions {
  onMissing?: () => Promise<void>;
}

function runOrThrow(cmd: string, args: string[], opts: RunOptions = {}): Promise<void> {
  return new Promise((resolve, reject) => {
    let stderr = "";
    const child = spawn(cmd, args);
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", async (err) => {
      const code = (err as NodeJS.ErrnoException).code;
      if (code === "ENOENT" && opts.onMissing) {
        try {
          await opts.onMissing();
          resolve();
        } catch (e) {
          reject(e);
        }
      } else {
        reject(err);
      }
    });
    child.on("close", (code) => {
      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`${cmd} exited ${code}: ${stderr.trim()}`));
      }
    });
  });
}

// =============================================================================
// API client
// =============================================================================

class PrintApi {
  constructor(private readonly cfg: AgentConfig) {}

  async heartbeat(): Promise<{ poll_interval_seconds: number; name: string }> {
    const res = await this.request("POST", "/api/print/heartbeat", {
      json: {
        agent_version: VERSION,
        system_printer: this.cfg.system_printer,
        host: hostname(),
      },
    });
    if (!res.ok) {
      throw new Error(`heartbeat failed: ${res.status} ${await res.text()}`);
    }
    const body = (await res.json()) as { data: { poll_interval_seconds: number; name: string } };
    return body.data;
  }

  async fetchNext(): Promise<NextJob | null> {
    const res = await this.request("GET", "/api/print/jobs/next");
    if (res.status === 204) return null;
    if (!res.ok) {
      throw new Error(`next failed: ${res.status} ${await res.text()}`);
    }
    const buffer = Buffer.from(await res.arrayBuffer());
    return {
      uuid: res.headers.get("X-Print-Job-Uuid") || "",
      format: res.headers.get("X-Print-Job-Format") || "pdf",
      tracking: res.headers.get("X-Print-Job-Tracking") || "",
      bytes: buffer,
    };
  }

  async ack(uuid: string, status: "printed" | "failed", error?: string, durationMs?: number): Promise<void> {
    const res = await this.request("POST", `/api/print/jobs/${uuid}/ack`, {
      json: { status, error, duration_ms: durationMs },
    });
    if (!res.ok) {
      throw new Error(`ack failed: ${res.status} ${await res.text()}`);
    }
  }

  private async request(method: string, path: string, opts: { json?: unknown } = {}): Promise<Response> {
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.cfg.printer_token}`,
      Accept: "application/json, application/pdf, application/octet-stream",
      "User-Agent": USER_AGENT,
    };
    let body: string | undefined;
    if (opts.json !== undefined) {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify(opts.json);
    }
    return fetch(`${this.cfg.api_url.replace(/\/$/, "")}${path}`, { method, headers, body });
  }
}

interface NextJob {
  uuid: string;
  format: string;
  tracking: string;
  bytes: Buffer;
}

// =============================================================================
// Run loop
// =============================================================================

async function run(): Promise<void> {
  const cfg = loadConfig();
  if (!cfg) {
    console.error("No config found. Run `lesscommerce-print-agent setup` first.");
    process.exit(1);
  }

  console.log(`LessCommerce Print Agent v${VERSION}`);
  console.log(`API: ${cfg.api_url}`);
  console.log(`System printer: ${cfg.system_printer}`);
  console.log("");

  const api = new PrintApi(cfg);
  let pollIntervalSec = cfg.poll_interval || 5;

  // Hard-stop hooks — Ctrl-C / SIGTERM (systemd, launchd) flush any in-flight
  // log line before exiting so the operator's last message isn't truncated.
  let stopping = false;
  const shutdown = (signal: string) => {
    if (stopping) return;
    stopping = true;
    console.log(`\nReceived ${signal}, stopping…`);
    process.exit(0);
  };
  process.on("SIGINT", () => shutdown("SIGINT"));
  process.on("SIGTERM", () => shutdown("SIGTERM"));

  while (!stopping) {
    try {
      const heartbeat = await api.heartbeat();
      pollIntervalSec = Math.max(1, heartbeat.poll_interval_seconds || pollIntervalSec);
    } catch (err) {
      console.error(`[heartbeat] ${(err as Error).message}`);
      // Back off a bit on failure but keep trying.
      await sleep(Math.min(pollIntervalSec * 2, 30) * 1000);
      continue;
    }

    try {
      const job = await api.fetchNext();
      if (!job) {
        await sleep(pollIntervalSec * 1000);
        continue;
      }

      console.log(`[job] ${job.uuid.slice(0, 8)}… tracking=${job.tracking} format=${job.format} bytes=${job.bytes.length}`);
      const t0 = Date.now();
      try {
        await printLabel(job.bytes, job.format, cfg.system_printer);
        const dur = Date.now() - t0;
        await api.ack(job.uuid, "printed", undefined, dur);
        console.log(`[job] ${job.uuid.slice(0, 8)}… printed in ${dur}ms`);
      } catch (err) {
        const message = (err as Error).message;
        console.error(`[job] ${job.uuid.slice(0, 8)}… FAILED: ${message}`);
        try {
          await api.ack(job.uuid, "failed", message);
        } catch (ackErr) {
          console.error(`[job] ack-failed also failed: ${(ackErr as Error).message}`);
        }
      }

      // Process the next job immediately rather than sleeping — burst printing
      // (10 orders queued at once) shouldn't wait 5s between each.
    } catch (err) {
      console.error(`[loop] ${(err as Error).message}`);
      await sleep(pollIntervalSec * 1000);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

// =============================================================================
// Setup wizard
// =============================================================================

async function setup(): Promise<void> {
  console.log(`LessCommerce Print Agent setup v${VERSION}\n`);

  const apiUrl = await prompt("API URL", "https://api.lesscommerce.io");
  const token = await prompt("Printer token (lpa_live_…)");
  if (!token.startsWith("lpa_")) {
    console.warn("Token does not start with `lpa_` — proceeding anyway.");
  }

  console.log("\nDetecting system printers…");
  let printers: string[] = [];
  try {
    printers = await listSystemPrinters();
  } catch (err) {
    console.warn(`Could not list printers automatically: ${(err as Error).message}`);
  }

  if (printers.length > 0) {
    console.log("Available printers:");
    printers.forEach((p, i) => console.log(`  [${i + 1}] ${p}`));
  }

  const printerChoice = await prompt("System printer name (or number from list)", printers[0] ?? "");
  let systemPrinter = printerChoice;
  const asNumber = parseInt(printerChoice, 10);
  if (!isNaN(asNumber) && asNumber >= 1 && asNumber <= printers.length) {
    systemPrinter = printers[asNumber - 1] as string;
  }

  const pollInterval = parseInt(await prompt("Poll interval (seconds)", "5"), 10) || 5;

  const cfg: AgentConfig = {
    api_url: apiUrl,
    printer_token: token,
    system_printer: systemPrinter,
    poll_interval: pollInterval,
  };

  saveConfig(cfg);
  console.log(`\nSaved config to ${configPath()}`);
  console.log(`\nTesting connection…`);
  try {
    const heartbeat = await new PrintApi(cfg).heartbeat();
    console.log(`✔  Connected as printer "${heartbeat.name}". Server poll interval: ${heartbeat.poll_interval_seconds}s.`);
    console.log(`\nStart the agent with:  lesscommerce-print-agent run`);
  } catch (err) {
    console.error(`✗  Connection test failed: ${(err as Error).message}`);
    console.error(`Config saved anyway. Fix the API URL or token and re-run setup.`);
    process.exit(2);
  }
}

function prompt(label: string, defaultVal?: string): Promise<string> {
  return new Promise((resolve) => {
    const dflt = defaultVal !== undefined && defaultVal !== "" ? ` [${defaultVal}]` : "";
    process.stdout.write(`${label}${dflt}: `);
    const onData = (chunk: Buffer) => {
      process.stdin.off("data", onData);
      const v = chunk.toString().trim();
      resolve(v || defaultVal || "");
    };
    process.stdin.on("data", onData);
  });
}

// =============================================================================
// CLI dispatch
// =============================================================================

async function main(): Promise<void> {
  const cmd = process.argv[2] || "help";

  switch (cmd) {
    case "run":
      await run();
      break;
    case "setup":
      await setup();
      process.exit(0);
    // Falls through intentionally
    case "list-printers": {
      const ps = await listSystemPrinters();
      ps.forEach((p) => console.log(p));
      process.exit(0);
    }
    // Falls through intentionally
    case "version":
    case "--version":
    case "-v":
      console.log(VERSION);
      process.exit(0);
    // Falls through intentionally
    default:
      console.log(`LessCommerce Print Agent v${VERSION}

Usage:
  lesscommerce-print-agent setup           Interactive setup (API URL, token, printer)
  lesscommerce-print-agent run             Run the polling loop (foreground)
  lesscommerce-print-agent list-printers   List system printers
  lesscommerce-print-agent version         Print version

Config file: ${configPath()}
`);
      process.exit(cmd === "help" ? 0 : 1);
  }
}

main().catch((err) => {
  console.error("Fatal:", err);
  process.exit(99);
});

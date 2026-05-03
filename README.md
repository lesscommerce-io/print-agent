# LessCommerce Print Agent

Lekki demon (~50 MB binarka), który łączy lokalną drukarkę kodów kreskowych z chmurową kolejką druku w LessCommerce. Po instalacji i konfiguracji odpytuje API co 5 sekund o nowe etykiety, drukuje je na podpiętej drukarce (Zebra, Brother QL, DYMO …) i potwierdza.

## Wymagania

- **Linux**: zainstalowany CUPS (`lp` w PATH). Drukarka rozpoznana przez system (`lpstat -p`).
- **macOS**: jak Linux — CUPS jest preinstalowany.
- **Windows**: drukarka zainstalowana w „Devices and Printers". Zalecane: [SumatraPDF](https://www.sumatrapdfreader.org/) w PATH (do druku PDF; bez niego agent użyje `Start-Process -Verb PrintTo`, ale SumatraPDF jest deterministyczny).

## Instalacja

Pobierz binarkę dla swojego systemu z [GitHub Releases](https://github.com/lesscommerce-io/print-agent/releases/latest) i umieść w PATH (`/usr/local/bin/`, `~/bin/` lub `C:\Program Files\LessCommerce\`).

## Konfiguracja

```bash
lesscommerce-print-agent setup
```

Wizard zapyta o:
1. **API URL** (zazwyczaj `https://api.lesscommerce.io`)
2. **Token drukarki** — wygenerowany w panelu admina (Ustawienia sklepu → Drukarki etykiet → Dodaj drukarkę)
3. **Drukarkę systemową** — wybierasz z listy wykrytej automatycznie
4. **Poll interval** — domyślnie 5 sekund

Konfiguracja trafia do:
- Linux/macOS: `~/.config/lesscommerce/print-agent.json`
- Windows: `%APPDATA%\lesscommerce\print-agent.json`

Po setupie agent zrobi heartbeat i potwierdzi że token + drukarka działają.

## Uruchamianie

### Foreground (do testów)

```bash
lesscommerce-print-agent run
```

Polling loop loguje na stdout:
```
LessCommerce Print Agent v0.1.0
API: https://api.lesscommerce.io
System printer: Zebra ZD220

[job] a3f1d8e0… tracking=628012345678 format=pdf bytes=14623
[job] a3f1d8e0… printed in 1842ms
```

Ctrl-C zatrzymuje.

### Auto-start (linux)

systemd unit:
```ini
# /etc/systemd/system/lesscommerce-print-agent.service
[Unit]
Description=LessCommerce Print Agent
After=cups.service network-online.target
Wants=cups.service

[Service]
Type=simple
ExecStart=/usr/local/bin/lesscommerce-print-agent run
User=admin
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now lesscommerce-print-agent
sudo journalctl -u lesscommerce-print-agent -f
```

### Auto-start (macOS)

LaunchAgent w `~/Library/LaunchAgents/io.lesscommerce.print-agent.plist`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>io.lesscommerce.print-agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/lesscommerce-print-agent</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/lesscommerce-print-agent.log</string>
  <key>StandardErrorPath</key><string>/tmp/lesscommerce-print-agent.err</string>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/io.lesscommerce.print-agent.plist
```

### Auto-start (Windows)

Najprostsza opcja — Task Scheduler:
1. Otwórz „Task Scheduler" → „Create Basic Task"
2. Trigger: „When I log on"
3. Action: „Start a program" → `C:\Program Files\LessCommerce\lesscommerce-print-agent.exe` z argumentem `run`
4. „Run whether user is logged on or not" + „Run with highest privileges"

Albo zarejestruj jako service przez [NSSM](https://nssm.cc/):
```cmd
nssm install LessCommercePrintAgent "C:\Program Files\LessCommerce\lesscommerce-print-agent.exe" run
nssm start LessCommercePrintAgent
```

## Komendy CLI

| Komenda | Opis |
|---|---|
| `setup` | Interaktywny wizard konfiguracji |
| `run` | Uruchamia polling loop (foreground) |
| `list-printers` | Listuje drukarki widoczne w systemie |
| `version` | Wyświetla wersję |

## Format etykiet

Agent obsługuje:
- **PDF** (default; InPost, DPD i wszystko inne) — drukowane przez SumatraPDF / `lp`
- **ZPL/EPL** (raw — InPost ShipX wspiera) — wysyłane bajt-po-bajcie do drukarki przez `lp -o raw` / `Out-Printer`

Format jest dyktowany przez serwer per-job (header `X-Print-Job-Format`). Agent nie wybiera.

## Bezpieczeństwo

- Token przechowywany jest plain-text w pliku konfiguracyjnym lokalnie. Chrońmy ten plik OS-owymi ACL-ami (Linux/macOS — `chmod 600`, Windows — domyślne pliki w `%APPDATA%` są user-only).
- Token nigdy nie wraca z serwera — jest pokazany **raz** przy tworzeniu drukarki w panelu. Stracony = wymień token (button „Wymień token" w panelu) i przekonfiguruj agenta.
- Komunikacja zawsze przez HTTPS. Agent waliduje cert (Bun fetch).

## Troubleshooting

### „No config found"

Nie odpaliłeś `setup` albo plik konfiguracyjny zniknął. Odpal setup ponownie.

### „heartbeat failed: 401"

Token został wymieniony albo drukarka usunięta. Wygeneruj nowy token w panelu i odpal `setup` ponownie.

### „lp exited 1: No such file or directory"

Drukarka systemowa o tej nazwie nie istnieje. `lesscommerce-print-agent list-printers` pokaże dostępne, potem odpal setup ponownie.

### Druk PDF na Windows pyta o aplikację

Brakuje SumatraPDF — albo ją zainstaluj (zalecane), albo Adobe Acrobat ustaw jako default dla `.pdf`. Inaczej agent użyje fallbacka `Start-Process` co czasem pokazuje preview okno.

### Job stuck w „claimed"

Agent wziął zlecenie ale nie zakwitował (crash, sieć padła). Po restarcie agent **nie** wraca do tego joba — admin musi w panelu kliknąć „Ponów" w kolejce druku albo poczekać aż timeout cleanup go zwróci do pendingu (TODO).

## Development

```bash
bun install
bun run dev setup
bun run dev run
```

Build cross-platform:
```bash
bun run build:all   # all 5 binaries → dist/
```

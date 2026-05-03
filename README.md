# LessCommerce Print Agent

Lekka aplikacja desktopowa (~15 MB) która łączy lokalną drukarkę kodów kreskowych z chmurową kolejką druku w LessCommerce. Po instalacji siedzi w pasku systemowym (system tray), odpytuje API co 5 sekund o nowe etykiety, drukuje je na podpiętej drukarce (Zebra, Brother QL, DYMO, …) i potwierdza.

## Pobieranie

| OS | Plik | Notatki |
|---|---|---|
| **Windows** | `.msi` (zalecany) lub `.exe` (NSIS portable) | Rejestruje deep-link i autostart |
| **macOS** | `.dmg` (Apple Silicon — `aarch64`, Intel — `x64`) | Pierwsze uruchomienie: prawy klik → Open |
| **Linux** | `.AppImage` lub `.deb` | AppImage = jeden plik, bez instalacji |

[**Najnowsza wersja → GitHub Releases**](https://github.com/lesscommerce-io/print-agent/releases/latest)

Po ściągnięciu zainstaluj klikając installer (Windows/Mac), albo przeciągnij `.app` do `Applications` (Mac DMG), albo `chmod +x foo.AppImage && ./foo.AppImage` (Linux).

## Pierwsze uruchomienie

1. Otwórz aplikację — pojawi się okno setupu.
2. W panelu LessCommerce kliknij **Ustawienia sklepu → Drukarki etykiet → Dodaj drukarkę**, a potem **„Otwórz w Print Agent"** w bannerze z tokenem.
3. Aplikacja sama wczyta token i adres API. Wybierz drukarkę systemową z dropdownu, zaznacz „Uruchamiaj automatycznie po zalogowaniu" i kliknij **Zapisz i uruchom**.
4. Okno chowa się do trayu — od teraz prawy klik na ikonę → menu z opcjami, lewy klik → przywraca okno.

Konfiguracja zapisywana jest w:
- Linux: `~/.config/lesscommerce/print-agent.json`
- macOS: `~/Library/Application Support/io.lesscommerce.print-agent/print-agent.json`
- Windows: `%APPDATA%\lesscommerce\print-agent.json`

## Format etykiet

- **PDF** (default; InPost, DPD i wszystko inne) — drukowane przez SumatraPDF (Win) / `lp` (Linux/macOS)
- **ZPL/EPL** (raw) — wysyłane bajt-po-bajcie do drukarki przez `lp -o raw` / `Out-Printer`

Format dyktowany przez serwer per-job (`X-Print-Job-Format` header).

## Wymagania systemowe

- **Linux**: zainstalowany CUPS (`lp` w PATH). WebKit2GTK 4.1+ (preinstalowany w Ubuntu 22.04+, Fedora 36+).
- **macOS**: 10.15+. CUPS preinstalowany.
- **Windows**: 10/11. Zalecane: [SumatraPDF](https://www.sumatrapdfreader.org/) w PATH (deterministyczne silent printing PDF). Bez niego agent użyje `Start-Process -Verb PrintTo` jako fallback.

## Pierwsze uruchomienie — ostrzeżenia OS

Aplikacja nie jest podpisana cyfrowo (świadomy wybór: 0 PLN kosztów vs $400/rok za certyfikaty). Konsekwencje:

- **Windows**: SmartScreen pokaże „unrecognized app". Klik **More info → Run anyway**. OS zapamięta wybór.
- **macOS**: pierwszy klik → „App can't be opened because it is from an unidentified developer". Idź do **Ustawienia → Privacy & Security → Open Anyway**, albo prawy klik na app → **Open** w dialogu.
- **Linux**: brak warning'a. AppImage / .deb po prostu działają.

## Bezpieczeństwo

- Token przechowywany jest plain-text w pliku konfiguracyjnym lokalnie. Chrońmy ten plik OS-owymi ACL-ami (Linux/macOS — `chmod 600`, Windows — domyślne pliki w `%APPDATA%` są user-only).
- Token nigdy nie wraca z serwera — jest pokazany **raz** przy tworzeniu drukarki w panelu. Stracony = wymień token (button „Wymień token" w panelu) i przekonfiguruj agenta.
- Komunikacja zawsze przez HTTPS.

## Troubleshooting

### Aplikacja nie wykrywa drukarki

Linux/Mac: sprawdź `lpstat -p` w terminalu — drukarka musi tam być widoczna. Jeśli nie ma, dodaj ją w System Settings → Printers.

Windows: sprawdź „Devices and Printers" w Panelu sterowania.

W obu przypadkach kliknij ↻ obok dropdownu w setupie żeby odświeżyć listę.

### Status „Error" z komunikatem 401

Token został wymieniony lub drukarka usunięta w panelu. Wygeneruj nowy token i otwórz nowy „Otwórz w Print Agent" link, albo wklej token ręcznie do okna setupu.

### Zadania utykają w „claimed"

To znaczy że agent wziął zlecenie ale go nie zakwitował (crash, sieć padła). W panelu LessCommerce → Wysyłki → Kolejka druku zobaczysz badge **„Zacięte"** po 5 minutach. Klik **„Ponów"** zwraca zadanie do `pending` — ten sam agent je weźmie w następnym pollu.

### Druk PDF na Windows pyta o aplikację

Brakuje SumatraPDF — zainstaluj ją (zalecane dla termo-drukarek), albo ustaw Adobe Acrobat jako default dla `.pdf`.

## Development

```bash
# Wymagania: Rust 1.77+, Node.js (dla niczego — frontend jest pure HTML),
# WebKit2GTK na Linux (sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev)

cargo install tauri-cli --version "^2.0" --locked

# Dev mode — odpala Tauri z hot reload
cd src-tauri && cargo tauri dev

# Production build dla obecnego OS
cd src-tauri && cargo tauri build

# Output:
#   target/release/bundle/macos/*.app, *.dmg
#   target/release/bundle/appimage/*.AppImage
#   target/release/bundle/deb/*.deb
#   target/release/bundle/msi/*.msi
#   target/release/bundle/nsis/*.exe
```

Cross-build dla wszystkich OS-ów robi GitHub Actions na tag `v*` push (`.github/workflows/release.yml`).

## Architektura

```
src-tauri/         # Rust backend
├── src/
│   ├── main.rs      # Entry point — calls lib.rs::run()
│   ├── lib.rs       # Tauri builder + IPC commands + tray + deep-link
│   ├── api.rs       # HTTP client (heartbeat / fetch_next / ack)
│   ├── config.rs    # Load/save print-agent.json
│   ├── poller.rs    # Polling loop (tokio task)
│   └── printer.rs   # Discovery + send-to-printer (CUPS / Get-Printer / SumatraPDF)
├── icons/           # App + tray icons (PNG, ICNS, ICO)
├── tauri.conf.json  # Window/bundle/plugin config
└── capabilities/    # IPC permissions (Tauri 2 security model)

ui/                  # Frontend — vanilla HTML/CSS/JS, no build step
├── index.html       # Setup window
├── main.js          # IPC calls + DOM
└── styles.css       # Light + dark mode
```

Logika polling loopa w `poller.rs`:
1. **heartbeat** — bumpy `last_seen_at` server-side, server zwraca poll cadence
2. **fetch_next** — atomowy claim joba (na BE: `SELECT … FOR UPDATE SKIP LOCKED`); 204 = pusta kolejka
3. **print** — temp file + lp/SumatraPDF
4. **ack** — `printed` z duration_ms, albo `failed` z error message

Status emitowany przez Tauri event `poller-status` do okna setupu (live UI: kropka koloru + komunikat + licznik wydrukowanych/błędów).

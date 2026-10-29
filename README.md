# PB2 Launcher (Tauri)

Desktop launcher for the HTML5 Physics Burst build. It downloads the game
files from the server into a local folder, keeps them in sync, and plays the
game in a native window — the game itself still runs as JavaScript, in the
system webview.

Roughly 5–10 MB installed, against ~100 MB for the Electron equivalent, because
it uses the OS webview (WebView2 / WKWebView / WebKitGTK) instead of shipping
Chromium.

## Build

```bash
cargo build --release --manifest-path src-tauri/Cargo.toml
```

The binary lands in `src-tauri/target/release/pb2-launcher`. Copy
`config.json` next to it, and — for a GNU build — `WebView2Loader.dll` as well;
the exe, the config and the DLL are the whole distribution.

`WebView2Loader.dll` is not optional. It is a hard import of the executable,
not a delay-load: without it in the executable's own directory the process
dies at start-up with `STATUS_DLL_NOT_FOUND` (`0xC0000135`) and no window. It
is the small loader that finds and loads the WebView2 runtime the machine
already has — `EmbeddedBrowserWebView.dll` under
`C:\Program Files (x86)\Microsoft\EdgeWebView\Application\<version>`, which
the loader opens once it has run. Ship the copy from the repository root
(1.0.3800.47, matching the WebView2 SDK this build was made against) rather
than a random one, and keep it beside the exe, not in a subfolder.

A build made with the MSVC toolchain links the same loader statically and needs
no DLL — `webview2-com-sys` picks `WebView2LoaderStatic` whenever
`target_env = "msvc"`. The releases here are built with the GNU toolchain
(`x86_64-pc-windows-gnu`), so the DLL stays.

For a distributable installer:

```bash
cargo install tauri-cli --version "^2"
cargo tauri build
```

### Icons

`src-tauri/icons/` holds one set for every platform, all cut from the same
square artwork: `32x32.png`, `128x128.png`, `128x128@2x.png` and `icon.png`
(512) for the window and for Linux, `icon.ico` (16–256) for the Windows
executable, and `icon.icns` (up to 1024) for the macOS bundle. `tauri.conf.json`
lists all of them, so `cargo tauri build` has what each platform asks for and
macOS no longer fails on a missing `.icns`.

To replace the artwork, start from a square PNG of at least 512×512 and run
`cargo tauri icon path/to/art.png`; it writes the whole set into
`src-tauri/icons/`. Nothing else needs to change, because the file names stay
the same.

## Configure

`config.json`, next to the executable:

```json
{
  "server": "https://your-domain",
  "accounts": null
}
```

- `server` — the game server (serves `index.html`, `/ws`, `/manifest.json`).
  A bare host — `your-domain`, no scheme — is read as `https://your-domain`,
  because the public server is published behind a TLS edge. Write `http://`
  only for the local dev server or a LAN host that has no certificate; an edge
  that only answers https still lifts those with a redirect.
- `accounts` — account service. `null` (default) means "same host and scheme,
  port 2054" — `https://your-domain:2054` for an https server. If the account
  service is published somewhere else (another host, a path on the game
  origin), set the full URL here; `false` disables sign-in, and guests still
  work.

`PB2_SERVER` in the environment overrides `server`, and is normalized the same
way.

Certificates are checked against the OS trust store (SChannel on Windows,
Security.framework on macOS, the system CA bundle on Linux) — there is no
pinned or bundled root, and no way to skip verification from the config. A
host whose chain does not validate fails at the update check with the TLS
error, before anything is written to disk.

## How it works

1. **Update check.** `GET <server>/manifest.json` returns every file with its
   size and SHA-256. The launcher compares that against the local copy and
   downloads only the files whose hash differs. Files that already match are
   not touched — a second start transfers nothing. An `https://` server is
   fetched over TLS through the platform stack (ureq + native-tls), so the
   manifest and every game file are verified against the OS trust store.
2. **Local copy.** Files land in the platform app-data folder:
   - Windows: `%APPDATA%/com.pb2.launcher/game`
   - macOS: `~/Library/Application Support/com.pb2.launcher/game`
   - Linux: `~/.config/com.pb2.launcher/game`

   Hash results are memoized in `files.json` beside it, keyed by size and
   mtime, so a warm start costs one `stat` per file instead of re-reading
   60 MB.
3. **Window.** The folder is served over `http://localhost:47821` and the
   window navigates there. A real HTTP origin is used instead of a custom
   scheme because the client relies on `fetch()`/XHR, MIME types and byte
   ranges; the loopback server answers all three (including `Range` for the
   audio tracks).

   The port is **fixed** on purpose. The game saves the player name, the
   settings and the account session in `localStorage`, which is keyed by
   origin — scheme, host *and port*. An OS-assigned port gives every launch a
   different origin, so each start began with empty storage and the name and
   settings were lost. `47821` is below the ephemeral range, so only a local
   service that deliberately took it can collide; in that case the launcher
   falls back to an OS-assigned port and still runs, just without the saved
   data.
4. **Server address.** The page's origin is the loopback server, so
   `__PB2_MP_SERVER_URL` and `__PB2_ACCOUNTS_URL` are pinned to the real
   addresses by an initialization script that runs before any page script. The
   game's own origin-based auto-detect assigns to them in sloppy mode, and
   assigning to a non-writable property is a silent no-op, so the pinned values
   win. The same script pins `__PB2_DEVICE`, the machine identifiers below.

   The scheme travels with the pinned address: the client turns `https://` into
   `wss://` and `http://` into `ws://` when it opens the socket, so an https
   server is what puts the multiplayer session on TLS. The page itself stays on
   `http://localhost`, which the webview already treats as a secure context;
   only the traffic to the game server is encrypted, and `wss://` from an
   `http://localhost` page is not blocked as mixed content.
5. **Machine identifiers.** Before the window exists, the launcher reads what a
   browser cannot: the SMBIOS UUID and BIOS serial (the firmware table on
   Windows, `/sys/class/dmi/id` on Linux, `ioreg` on macOS). They are pinned
   into the page as one JSON string and the game sends them back on its account
   requests, so the account service can keep them next to the address log — see
   `accounts/README.md` for what the service does with them.

   Only those two are read. A CPUID signature and a display adapter's PCI id
   name a model that whole product lines share, so they cannot tell two
   machines apart and are not collected.

   An identifier that cannot be read is left out rather than guessed at, and
   firmware placeholder ids are dropped (VMware's fixed UUID), because those
   would claim that every board from one vendor is one machine. Reading them
   needs no elevation and spawns no process on Windows.

   These are evidence, not enforcement: the player owns the machine and could
   report anything. The launcher does not use them for anything itself.

## Notes

- The downloaded folder is disposable: delete it and the next start fetches
  everything again.
- Files removed from the server are deleted locally on the next start.
- Only changed files are transferred, so an update is usually a few KB.
- The launcher refuses any manifest path that would resolve outside the game
  folder.

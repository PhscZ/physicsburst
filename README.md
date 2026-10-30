# Physics Burst Launcher (Tauri)

Desktop launcher for the HTML5 Physics Burst build. It downloads the game
files from the server into a local folder, keeps them in sync, and plays the
game in a native window, the game itself still runs as JavaScript, just under the
system webview.

Roughly 5–10 MB installed, against ~100 MB for the Electron equivalent, because
it uses the OS webview (WebView2 / WKWebView / WebKitGTK) instead of shipping
Chromium.

## Build

```bash
cargo build --release --manifest-path src-tauri/Cargo.toml
```

The binary lands in `src-tauri/target/release/pb`. Copy
`config.json` next to it, and — for a GNU build — `WebView2Loader.dll` as well;
the exe, the config and the DLL are the whole distribution.

`config.json` is optional: without it the launcher uses the server compiled in
(`https://physicsburst.ddns.net`), so a bare exe still reaches the public
server. Put a `config.json` beside it only to point somewhere else — a local
dev server, say. `PB2_SERVER` overrides both. Only the copy in the exe's own
folder counts; one in a parent folder is ignored.

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

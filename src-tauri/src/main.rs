//! Tauri shell for the Physics Burst HTML5 build.
//!
//! Startup:
//!   1. read config.json (or PB2_SERVER) for the game server address
//!   2. sync the game files into <app data>/game (only changed files are fetched)
//!   3. serve that folder over http://localhost:47821 (fixed port, see
//!      `GAME_PORT` — the origin keys the game's saved name and settings)
//!   4. navigate the window there — the game itself still runs as JavaScript
//!
//! Why a loopback HTTP server instead of Tauri's asset protocol: the client
//! uses `fetch()`/XHR for its own data and expects normal HTTP semantics
//! (MIME types, ranges, 404s). A real origin on 127.0.0.1 gives it exactly
//! that, and WebSocket traffic to the game server is unaffected by CORS.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod device;
mod sync;

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

// ---- Config --------------------------------------------------------------

struct Config {
    server: String,
    accounts: String,
}

fn read_config() -> Config {
    // config.json sits next to the executable, so an operator can edit it
    // without rebuilding. PB2_SERVER wins when set (handy for testing).
    let mut cfg: serde_json::Value = serde_json::Value::Null;
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [dir.join("config.json"), dir.join("..").join("config.json")] {
                if let Ok(raw) = fs::read_to_string(&candidate) {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) {
                        cfg = parsed;
                        break;
                    }
                }
            }
        }
    }

    let server = std::env::var("PB2_SERVER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            cfg.get("server")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "http://localhost:2053".to_string());
    let server = server.trim().trim_end_matches('/').to_string();

    // The account service is a separate process on port 2054 of the same host.
    // `"accounts": false` disables sign-in, which leaves guest play working.
    let accounts = match cfg.get("accounts") {
        Some(serde_json::Value::Bool(false)) => String::new(),
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            s.trim().trim_end_matches('/').to_string()
        }
        _ => derive_accounts_url(&server),
    };

    Config { server, accounts }
}

fn derive_accounts_url(server: &str) -> String {
    let (scheme, rest) = match server.split_once("://") {
        Some(parts) => parts,
        None => return String::new(),
    };
    let host = rest.split('/').next().unwrap_or("");
    let hostname = host.split(':').next().unwrap_or(host);
    if hostname.is_empty() {
        return String::new();
    }
    format!("{scheme}://{hostname}:2054")
}

// ---- Local static server -------------------------------------------------

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "otf" => "font/otf",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "xml" => "application/xml",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Lexical normalisation, so a `..` in the request cannot escape the root.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn serve_one(root: &Path, request: tiny_http::Request) {
    let raw_url = request.url().to_string();
    let path_part = raw_url.split('?').next().unwrap_or("/");
    let decoded = percent_decode(path_part);
    let rel = if decoded == "/" {
        "index.html".to_string()
    } else {
        decoded.trim_start_matches('/').to_string()
    };

    let root_norm = normalize(root);
    let abs = normalize(&root.join(&rel));
    if abs != root_norm && !abs.starts_with(&root_norm) {
        let _ = request.respond(tiny_http::Response::empty(403));
        return;
    }

    let meta = match fs::metadata(&abs) {
        Ok(m) if m.is_file() => m,
        _ => {
            let _ = request.respond(tiny_http::Response::empty(404));
            return;
        }
    };

    let ext = abs
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let content_type = tiny_http::Header::from_bytes(
        &b"Content-Type"[..],
        mime_for(&ext).as_bytes(),
    )
    .expect("static header");

    let is_head = request.method() == &tiny_http::Method::Head;
    let length = meta.len();

    if is_head {
        let response = tiny_http::Response::empty(200)
            .with_header(content_type)
            .with_header(
                tiny_http::Header::from_bytes(&b"Content-Length"[..], length.to_string().as_bytes())
                    .expect("static header"),
            );
        let _ = request.respond(response);
        return;
    }

    let mut file = match fs::File::open(&abs) {
        Ok(f) => f,
        Err(_) => {
            let _ = request.respond(tiny_http::Response::empty(500));
            return;
        }
    };

    // Audio elements ask for byte ranges; answer them so seeking and the
    // initial decode of the long tracks work the same as on the real server.
    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    if let Some(spec) = range {
        if let Some((start, end)) = parse_range(&spec, length) {
            let mut ranged = file;
            if ranged.seek(SeekFrom::Start(start)).is_ok() {
                let count = (end - start + 1) as usize;
                let mut buf = vec![0u8; count];
                let mut read = 0usize;
                while read < count {
                    match ranged.read(&mut buf[read..]) {
                        Ok(0) => break,
                        Ok(n) => read += n,
                        Err(_) => break,
                    }
                }
                buf.truncate(read);
                let response = tiny_http::Response::from_data(buf)
                    .with_status_code(206)
                    .with_header(content_type)
                    .with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Range"[..],
                            format!("bytes {}-{}/{}", start, end, length).as_bytes(),
                        )
                        .expect("static header"),
                    );
                let _ = request.respond(response);
                return;
            }
            file = ranged;
        }
    }

    let response = tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![content_type],
        file,
        Some(length as usize),
        None,
    );
    let _ = request.respond(response);
}

fn parse_range(spec: &str, length: u64) -> Option<(u64, u64)> {
    let rest = spec.strip_prefix("bytes=")?;
    let (start_s, end_s) = rest.split_once('-')?;
    let start: u64 = start_s.trim().parse().ok()?;
    let end: u64 = if end_s.trim().is_empty() {
        length.saturating_sub(1)
    } else {
        end_s.trim().parse().ok()?
    };
    if start >= length || end < start {
        return None;
    }
    Some((start, end.min(length.saturating_sub(1))))
}

/// Port the game folder is served on.
///
/// The client persists the player name, the settings and the account session
/// in `localStorage` (`SharedObject.getLocal` is a thin wrapper around it, see
/// `game/scripts/flash.js`), and `localStorage` is keyed by **origin** —
/// scheme, host *and port*. An OS-assigned port (`:0`) therefore gives every
/// launch a different origin, so each start begins with empty storage and the
/// player loses their name, settings and login.
///
/// A fixed port keeps the origin identical across runs. It is below the
/// ephemeral range Windows hands to outgoing connections (49152+), so only a
/// local service that deliberately picked the same port can collide with it;
/// in that case the server falls back to an OS-assigned port and the launch
/// still works, just without the saved data.
const GAME_PORT: u16 = 47821;

/// Serve `root` on a loopback port. Returns the address.
fn start_static_server(root: PathBuf, shutdown: Arc<AtomicBool>) -> Result<SocketAddr, String> {
    let server = tiny_http::Server::http(("127.0.0.1", GAME_PORT))
        .or_else(|_| tiny_http::Server::http("127.0.0.1:0"))
        .map_err(|e| format!("could not start local file server: {e}"))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| "local file server: not an IP address".to_string())?;

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            serve_one(&root, request);
        }
    });

    Ok(addr)
}

fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn set_status(window: &tauri::WebviewWindow, text: &str, fraction: Option<f64>) {
    let mut script = format!(
        "var e=document.getElementById('s');if(e)e.textContent={};",
        js_string(text)
    );
    if let Some(f) = fraction {
        script.push_str(&format!(
            "var b=document.getElementById('f');if(b)b.style.width={};",
            js_string(&format!("{:.1}%", (f.clamp(0.0, 1.0)) * 100.0))
        ));
    }
    let _ = window.eval(&script);
}

fn show_error(window: &tauri::WebviewWindow, text: &str) {
    let script = format!(
        "var s=document.getElementById('s');if(s)s.textContent='Could not start';\
         var e=document.getElementById('err');if(e)e.textContent={};",
        js_string(text)
    );
    let _ = window.eval(&script);
}

// ---- Boot ----------------------------------------------------------------

fn boot(app: tauri::AppHandle) {
    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => return,
    };

    let config = read_config();
    let data_dir = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    let game_dir = data_dir.join("game");
    let state_file = data_dir.join("files.json");

    set_status(&window, "Checking for updates…", None);

    let mut on_progress = |done: usize, total: usize, _bytes: u64, _total_bytes: u64, current: &str| {
        let name = current.rsplit('/').next().unwrap_or(current);
        let pct = if total > 0 { done as f64 / total as f64 } else { 0.0 };
        set_status(
            &window,
            &format!("Downloading {done}/{total} — {name}"),
            Some(pct),
        );
    };

    let result = sync::sync_game(&config.server, &game_dir, &state_file, &mut on_progress);

    let summary = match result {
        Ok(r) => {
            if r.downloaded > 0 {
                format!("Updated {} file(s). Starting…", r.downloaded)
            } else {
                "Up to date. Starting…".to_string()
            }
        }
        Err(e) => {
            show_error(
                &window,
                &format!(
                    "{e}\n\nServer: {}\n\nCheck that the server is running and reachable.",
                    config.server
                ),
            );
            return;
        }
    };

    if !game_dir.join("index.html").is_file() {
        show_error(&window, "index.html is missing from the downloaded files.");
        return;
    }

    set_status(&window, &summary, Some(1.0));

    let shutdown = Arc::new(AtomicBool::new(false));
    let addr = match start_static_server(game_dir, shutdown) {
        Ok(addr) => addr,
        Err(e) => {
            show_error(&window, &e);
            return;
        }
    };

    // `localhost`, not the bound 127.0.0.1 address: Chromium's HTTPS-First
    // mode upgrades a literal `http://127.0.0.1:<port>/` to https:// and then
    // fails against this plain-HTTP server (ERR_TIMED_OUT). `localhost` is on
    // the upgrade exemption list, so the plain origin is used as-is.
    //
    // Both parts of this URL are load-bearing for persistence: the port comes
    // from `GAME_PORT` so the origin is stable, and the host stays `localhost`
    // for the same reason — switching between `127.0.0.1` and `localhost`
    // would be two different storage buckets.
    let url = format!("http://localhost:{}/", addr.port());
    if let Err(e) = window.navigate(url.parse().expect("valid url")) {
        show_error(&window, &format!("could not open the game: {e}"));
    }
}

fn main() {
    // WebView2 reads its Chromium switches from this variable at startup.
    // Tauri's `additional_browser_args` is applied too late for the HTTPS
    // upgrade decision in some WebView2 builds, so it is also set here — this
    // must happen before any webview is created.
    #[cfg(windows)]
    {
        let existing = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
        let ours = "--disable-features=HttpsUpgrades,HttpsFirstModeV2,\
                    HttpsFirstBalancedModeAutoEnable";
        let combined = if existing.is_empty() {
            ours.to_string()
        } else {
            format!("{existing} {ours}")
        };
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", combined);
    }

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let config = read_config();

            // The page's own origin is the loopback file server, so the
            // multiplayer address has to be pinned to the real server. The
            // inline patch in index.html assigns to these in sloppy mode, and
            // an assignment to a non-writable property is a silent no-op, so
            // these values win.
            // F11 is handled here rather than by the page: a browser does this
            // itself, but the launcher's WebView2 window has no browser chrome,
            // so nothing would intercept the key and it would do nothing.
            // Capture phase, registered before any page script, so the game's
            // own key handlers never see it.
            //
            // The machine identifiers are read once, here, rather than from
            // the page: a browser cannot see any of them. The game reads this
            // back as a JSON string and sends it with its account requests, so
            // the account service can keep it beside the address log.
            let device = js_string(&device::collect().to_string());

            let init = format!(
                "(function(){{\
                   function pin(n,v){{if(!v)return;try{{\
                     Object.defineProperty(window,n,{{value:v,writable:false,configurable:false,enumerable:true}});\
                   }}catch(e){{}}}};\
                   pin('__PB2_MP_SERVER_URL',{mp});\
                   pin('__PB2_ACCOUNTS_URL',{acc});\
                   pin('__PB2_DEVICE',{dev});\
                   function domToggle(){{\
                     if(document.fullscreenElement){{\
                       if(document.exitFullscreen)document.exitFullscreen();\
                     }}else if(document.documentElement.requestFullscreen){{\
                       var p=document.documentElement.requestFullscreen();\
                       if(p&&p.catch)p.catch(function(){{}});\
                     }}\
                   }}\
                   function toggle(){{\
                     try{{\
                       var t=window.__TAURI__&&window.__TAURI__.window;\
                       if(t&&t.getCurrentWindow){{\
                         var w=t.getCurrentWindow();\
                         w.isFullscreen().then(function(fs){{\
                           var r=w.setFullscreen(!fs);\
                           if(r&&r.catch)r.catch(domToggle);\
                         }}).catch(domToggle);\
                         return;\
                       }}\
                     }}catch(e){{}}\
                     domToggle();\
                   }}\
                   document.addEventListener('keydown',function(e){{
                     if(e.key!=='F11'&&e.keyCode!==122)return;\
                     e.preventDefault();
                     e.stopPropagation();
                     toggle();
                   }},true);\
                 }})();",
                mp = js_string(&config.server),
                acc = js_string(&config.accounts),
                dev = device,
            );

            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Physics Burst")
                .inner_size(1280.0, 720.0)
                .min_inner_size(800.0, 400.0)
                .background_color(tauri::window::Color(0, 0, 0, 255))
                .initialization_script(&init);

            // Chromium's HTTPS-First mode rewrites http://127.0.0.1:<port>/ to
            // https://, which the launcher's plain-HTTP loopback server cannot
            // answer (ERR_TIMED_OUT / ERR_SSL_PROTOCOL_ERROR). The local origin
            // only ever hands the game its own files, so the upgrade is both
            // pointless and fatal here.
            #[cfg(windows)]
            let builder = builder.additional_browser_args(
                "--disable-features=HttpsUpgrades,HttpsFirstModeV2,\
                 HttpsFirstBalancedModeAutoEnable",
            );

            builder.build()?;

            // Sync blocks on the network, so it runs off the UI thread.
            std::thread::spawn(move || boot(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// Baz Studios Launcher — one hub that keeps every Baz Studios game (and itself) up to date and
// runs it. It's a direct generalization of the WriftHeart launcher: each game is a pure-web
// bundle (index.html + js/), kept as plain static files in the OS app-data dir. On demand it asks
// GitHub for a game's latest release, and if that's newer than the local copy (or nothing is
// installed) it downloads the game's `*-game.zip` asset and unpacks it. "Play" serves that folder
// over a FIXED per-game localhost port and opens it in this window — a stable origin, so each
// game's localStorage saves persist across launches and updates.
//
// The games themselves know nothing about any of this. Delete the launcher and the downloaded
// files still run anywhere a browser/webview can open them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

// ---- The Baz Studios catalog (baked in) ----------------------------------------------------------
// Add a game: one row here. `repo` must be PUBLIC and each of its releases must attach `asset`
// (its web bundle, zipped) — a game with no such published release just shows "Coming soon". Each
// game gets its OWN fixed port so their saves never share an origin.
struct Game {
    slug: &'static str,    // stable id (folder name, UI key)
    name: &'static str,    // display name
    tagline: &'static str, // one-line blurb on the card
    repo: &'static str,    // owner/name on GitHub (must be public)
    asset: &'static str,   // the release asset the launcher downloads (the web bundle)
    port: u16,             // fixed localhost port -> stable save origin
    accent: &'static str,  // brand colour (hex) — drives the card's gradient / glow in the UI
}

const GAMES: &[Game] = &[
    Game {
        slug: "wriftheart",
        name: "WriftHeart",
        tagline: "An 8-bit action-RPG of a shattered world. Gather the ten shards; mend the Wriftheart.",
        repo: "Baz-Studios-LLC/wriftheart",
        asset: "wriftheart-game.zip",
        port: 47823,
        accent: "#b06cff",
    },
    Game {
        slug: "wingman",
        name: "Wingman",
        tagline: "A twin-stick shooter where you fly two ships at once.",
        repo: "Baz-Studios-LLC/Wingman",
        asset: "wingman-game.zip",
        port: 47824,
        accent: "#3a86ff",
    },
    Game {
        slug: "neondrift",
        name: "Neon Drift",
        tagline: "Neon-soaked arcade drifting on the edge of the grid.",
        repo: "Baz-Studios-LLC/Neon-Drift",
        asset: "neondrift-game.zip",
        port: 47825,
        accent: "#ff3bd0",
    },
];

fn game_by_slug(slug: &str) -> Option<&'static Game> {
    GAMES.iter().find(|g| g.slug == slug)
}

const UA: &str = "BazStudios-Launcher";

#[derive(Serialize)]
struct GameInfo {
    slug: String,
    name: String,
    tagline: String,
    repo: String,
    accent: String,
}

#[derive(Serialize)]
struct Latest {
    version: String,
    url: String,
    notes: String,
}

/// The baked-in catalog, handed to the UI so it renders one card per game from a single source.
#[tauri::command]
fn games() -> Vec<GameInfo> {
    GAMES
        .iter()
        .map(|g| GameInfo {
            slug: g.slug.to_string(),
            name: g.name.to_string(),
            tagline: g.tagline.to_string(),
            repo: g.repo.to_string(),
        })
        .collect()
}

// Where a game lives: <app_data_dir>/games/<slug>
fn game_dir(app: &tauri::AppHandle, slug: &str) -> Result<PathBuf, String> {
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(base.join("games").join(slug))
}

// Remembered fullscreen preference, so a game opens the way you left it. Written whenever the
// player toggles fullscreen (F11 / Cmd+Ctrl+F), read when Play launches a game.
fn fs_pref_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("fullscreen.pref"))
}
fn read_fs_pref(app: &tauri::AppHandle) -> bool {
    fs_pref_path(app)
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}
fn write_fs_pref(app: &tauri::AppHandle, on: bool) {
    if let Some(p) = fs_pref_path(app) {
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(p, if on { "1" } else { "0" });
    }
}

/// The LAUNCHER's own version (baked in at build time), shown in its footer.
#[tauri::command]
fn launcher_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// The version of a game's currently-installed copy (None if it isn't installed yet).
#[tauri::command]
fn installed_version(app: tauri::AppHandle, slug: String) -> Option<String> {
    let vf = game_dir(&app, &slug).ok()?.join("version.txt");
    fs::read_to_string(vf)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// Pull the download URL for a game's bundle asset out of a release's asset list, if present.
fn bundle_url(release: &serde_json::Value, asset: &str) -> Option<String> {
    release["assets"].as_array()?.iter().find_map(|a| {
        if a["name"].as_str() == Some(asset) {
            a["browser_download_url"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Find a game's newest release: scan its repo's release list (newest first) and take the first
/// published (non-draft, non-prerelease) release that actually carries its game bundle. Skips the
/// installer-only / launcher releases, so they never masquerade as "latest game".
#[tauri::command]
async fn check_latest(slug: String) -> Result<Latest, String> {
    let game = game_by_slug(&slug).ok_or("unknown game")?;
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .map_err(|e| e.to_string())?;
    let api = format!("https://api.github.com/repos/{}/releases?per_page=20", game.repo);
    let resp = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub returned {}", resp.status()));
    }
    let releases: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let list = releases.as_array().ok_or("unexpected GitHub response")?;
    for rel in list {
        if rel["draft"].as_bool() == Some(true) || rel["prerelease"].as_bool() == Some(true) {
            continue;
        }
        if let Some(url) = bundle_url(rel, game.asset) {
            let version = rel["tag_name"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches('v')
                .to_string();
            if version.is_empty() {
                continue;
            }
            let notes = rel["body"].as_str().unwrap_or("").to_string();
            return Ok(Latest { version, url, notes });
        }
    }
    Err("no published game release found".into())
}

/// Download a game's bundle and unpack it fresh into its game dir, then stamp the version.
#[tauri::command]
async fn install(
    app: tauri::AppHandle,
    slug: String,
    url: String,
    version: String,
) -> Result<(), String> {
    let dir = game_dir(&app, &slug)?;
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download returned {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;

    let dir2 = dir.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        if dir2.exists() {
            fs::remove_dir_all(&dir2).map_err(|e| e.to_string())?;
        }
        fs::create_dir_all(&dir2).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        archive.extract(&dir2).map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;

    fs::write(dir.join("version.txt"), version).map_err(|e| e.to_string())?;
    Ok(())
}

/// Launch a game by REUSING the launcher's own window — reconfigure it (bigger, decorated,
/// resizable) and navigate it to that game's local server. We deliberately do NOT open a second
/// WebviewWindow: on some Windows/WebView2 setups an additional window never paints. Each game is
/// served over its own fixed localhost port (a stable origin, so saves persist), started lazily on
/// first play and left running for the session.
#[tauri::command]
fn play(app: tauri::AppHandle, slug: String, serving: State<Serving>) -> Result<(), String> {
    let game = game_by_slug(&slug).ok_or("unknown game")?;
    let dir = game_dir(&app, &slug)?;
    if !dir.join("index.html").exists() {
        return Err("that game isn't installed yet".into());
    }
    {
        let mut bound = serving.0.lock().map_err(|_| "server lock poisoned".to_string())?;
        if !bound.contains(&game.port) {
            let pads = pad_state(&serving);
            serve(dir.clone(), game.port, pads, app.clone())?;
            bound.insert(game.port);
        }
    }
    let url: tauri::Url = format!("http://127.0.0.1:{}/", game.port)
        .parse()
        .map_err(|_| "bad game url".to_string())?;
    let win = app.get_webview_window("main").ok_or("no window")?;
    let _ = win.set_title(game.name);
    let _ = win.set_decorations(true);
    let _ = win.set_resizable(true);
    let _ = win.set_maximizable(true);
    let _ = win.set_size(tauri::LogicalSize::new(1280.0, 720.0));
    let _ = win.set_min_size(Some(tauri::LogicalSize::new(640.0, 360.0)));
    let _ = win.center();
    win.navigate(url).map_err(|e| e.to_string())?;
    let _ = win.set_focus();
    if read_fs_pref(&app) {
        let _ = win.set_fullscreen(true); // open the way the player last left it
    }
    Ok(())
}

// ---- Native controller bridge --------------------------------------------------------------------
// WKWebView (macOS) exposes no Gamepad API and WebView2 is inconsistent, so a controller that works
// in a browser is invisible to a game inside the launcher. We read it natively with gilrs on a
// thread and publish a W3C "standard"-mapping snapshot as JSON; each game polls /__gamepad and feeds
// it through navigator.getGamepads(). In a plain browser (no launcher server) the game just uses the
// real Gamepad API, so nothing changes there. One poller is shared by every game.
fn gp_button(pad: &gilrs::Gamepad, b: gilrs::Button) -> String {
    let pressed = pad.is_pressed(b);
    let v = pad.button_data(b).map(|d| d.value()).unwrap_or(if pressed { 1.0 } else { 0.0 });
    format!("{{\"pressed\":{},\"touched\":{},\"value\":{:.3}}}", pressed, pressed, v)
}
fn gp_axis(pad: &gilrs::Gamepad, a: gilrs::Axis) -> f32 {
    pad.axis_data(a).map(|d| d.value()).unwrap_or(0.0)
}
fn gp_json(index: usize, pad: &gilrs::Gamepad) -> String {
    use gilrs::{Axis, Button};
    let btns = [
        Button::South, Button::East, Button::West, Button::North,
        Button::LeftTrigger, Button::RightTrigger, Button::LeftTrigger2, Button::RightTrigger2,
        Button::Select, Button::Start, Button::LeftThumb, Button::RightThumb,
        Button::DPadUp, Button::DPadDown, Button::DPadLeft, Button::DPadRight, Button::Mode,
    ];
    let b: Vec<String> = btns.iter().map(|&x| gp_button(pad, x)).collect();
    let axes = [
        gp_axis(pad, Axis::LeftStickX), -gp_axis(pad, Axis::LeftStickY),
        gp_axis(pad, Axis::RightStickX), -gp_axis(pad, Axis::RightStickY),
    ];
    let a: Vec<String> = axes.iter().map(|v| format!("{:.3}", v)).collect();
    let id = pad.name().replace('\\', " ").replace('"', "'");
    format!(
        "{{\"index\":{},\"id\":\"{}\",\"mapping\":\"standard\",\"connected\":true,\"buttons\":[{}],\"axes\":[{}]}}",
        index, id, b.join(","), a.join(",")
    )
}
// Lazily start the gilrs poller (once) and return the shared JSON snapshot the servers hand out.
fn pad_state(serving: &State<Serving>) -> Arc<Mutex<String>> {
    let mut guard = serving.1.lock().unwrap();
    if let Some(s) = guard.as_ref() {
        return s.clone();
    }
    let state = Arc::new(Mutex::new(String::from("[]")));
    let inner = state.clone();
    std::thread::spawn(move || {
        let mut gilrs = match gilrs::Gilrs::new() {
            Ok(g) => g,
            Err(_) => return,
        };
        loop {
            while gilrs.next_event().is_some() {} // drain queued events so the state is current
            let mut parts: Vec<String> = Vec::new();
            for (i, (_id, pad)) in gilrs.gamepads().enumerate() {
                if pad.is_connected() {
                    parts.push(gp_json(i, &pad));
                }
            }
            let json = format!("[{}]", parts.join(","));
            if let Ok(mut s) = inner.lock() {
                *s = json;
            }
            std::thread::sleep(std::time::Duration::from_millis(12)); // ~80 Hz
        }
    });
    *guard = Some(state.clone());
    state
}

// A tiny static file server over one game's folder (its fixed localhost port) — the same thing the
// dev preview does, so the game behaves identically. Runs on its own thread for the life of the app.
// `pads` is the live controller snapshot, served at /__gamepad; /__quit returns to the library.
fn serve(dir: PathBuf, port: u16, pads: Arc<Mutex<String>>, app: tauri::AppHandle) -> Result<(), String> {
    let server = tiny_http::Server::http(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let raw = req.url().split('?').next().unwrap_or("/");
            let rel = raw.trim_start_matches('/');
            if rel == "__gamepad" {
                let body = pads.lock().map(|s| s.clone()).unwrap_or_else(|_| "[]".to_string());
                let ct = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                let _ = req.respond(tiny_http::Response::from_string(body).with_header(ct));
                continue;
            }
            if rel == "__quit" {
                // A game's "Exit" hits this — the web page can't drive the native window itself. We
                // relaunch the launcher so the player lands back on the library (a clean re-init, no
                // webview-bridge-reinjection guesswork).
                let _ = req.respond(tiny_http::Response::from_string("bye"));
                app.restart();
            }
            let rel = if rel.is_empty() { "index.html" } else { rel };
            let mut path = dir.clone();
            let mut safe = true;
            for part in rel.split('/') {
                if part == ".." || part.contains('\\') { safe = false; break; }
                if !part.is_empty() { path.push(part); }
            }
            if !safe || !path.is_file() {
                let _ = req.respond(tiny_http::Response::from_string("Not found").with_status_code(404));
                continue;
            }
            let ct = match path.extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "text/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("json") => "application/json; charset=utf-8",
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("gif") => "image/gif",
                Some("svg") => "image/svg+xml",
                Some("wav") => "audio/wav",
                Some("mp3") => "audio/mpeg",
                Some("ogg") => "audio/ogg",
                Some("woff2") => "font/woff2",
                _ => "application/octet-stream",
            };
            match fs::read(&path) {
                Ok(data) => {
                    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()).unwrap();
                    let _ = req.respond(tiny_http::Response::from_data(data).with_header(header));
                }
                Err(_) => { let _ = req.respond(tiny_http::Response::from_string("Read error").with_status_code(500)); }
            }
        }
    });
    Ok(())
}

// Session state: which game ports already have a server bound, + the shared gamepad snapshot.
struct Serving(Mutex<HashSet<u16>>, Mutex<Option<Arc<Mutex<String>>>>);

// ---- Self-update: the launcher keeps ITSELF current (separate from the games it manages). --------
#[tauri::command]
async fn self_update_check(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(update.version.clone())),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
async fn self_update_install(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    if let Some(update) = updater.check().await.map_err(|e| e.to_string())? {
        update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
            .map_err(|e| e.to_string())?;
        app.restart();
    }
    Ok(())
}

// Fullscreen shortcuts: F11 everywhere; Cmd+Ctrl+F on macOS (F11 is reserved by the OS there). wry
// ignores the HTML fullscreen API, so a game's in-page toggle can't resize the native window — we
// flip it from Rust instead. Registered only while our window is focused.
fn fs_shortcuts() -> Vec<Shortcut> {
    let mut v = vec![Shortcut::new(None, Code::F11)];
    #[cfg(target_os = "macos")]
    v.push(Shortcut::new(Some(Modifiers::SUPER | Modifiers::CONTROL), Code::KeyF));
    v
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build()) // the launcher keeps ITSELF up to date
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        if let Some(win) = app.get_webview_window("main") {
                            let on = win.is_fullscreen().unwrap_or(false);
                            let _ = win.set_fullscreen(!on);
                            write_fs_pref(app, !on);
                        }
                    }
                })
                .build(),
        )
        .manage(Serving(Mutex::new(HashSet::new()), Mutex::new(None)))
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { .. } => window.app_handle().exit(0),
            WindowEvent::Focused(focused) => {
                let gs = window.app_handle().global_shortcut();
                for s in fs_shortcuts() {
                    if *focused {
                        let _ = gs.register(s);
                    } else {
                        let _ = gs.unregister(s);
                    }
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            games,
            launcher_version,
            installed_version,
            check_latest,
            install,
            play,
            self_update_check,
            self_update_install
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Baz Studios launcher");
}

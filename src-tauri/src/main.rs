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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

// ---- The Baz Studios catalog (baked in) ----------------------------------------------------------
// Add a game: one row here. `repo` must be PUBLIC and each release must attach the delivery's asset.
// A game has one of two DELIVERIES:
//   * Web    — a pure-web bundle (index.html + js/, zipped). Downloaded, unpacked, and served over a
//              fixed localhost port INSIDE this launcher's webview (its own save origin).
//   * Native — a real platform executable (a Bevy game, etc.), per OS. Downloaded, unpacked, and
//              LAUNCHED as its own process; the game runs in its own window, this stays the library.
// Either way, no published release with the right asset = the card shows "Coming soon".
enum Delivery {
    /// A web bundle served in-webview over `port` (a stable save origin).
    Web { asset: &'static str, port: u16 },
    /// A native build per platform — the launcher installs and spawns it.
    Native { mac: &'static str, windows: &'static str },
}

impl Delivery {
    /// The release-asset SUFFIX to fetch for the platform we're running on (see `bundle_url` — assets
    /// are matched by suffix, so these can be stable tails like `-windows-x86_64.zip`).
    fn asset(&self) -> &'static str {
        match self {
            Delivery::Web { asset, .. } => asset,
            Delivery::Native { mac, windows } => native_pick(mac, windows),
        }
    }
}

/// The native asset for the current OS (mac / windows; falls back to the mac name elsewhere).
fn native_pick(mac: &'static str, windows: &'static str) -> &'static str {
    #[cfg(target_os = "macos")]
    {
        let _ = windows;
        mac
    }
    #[cfg(target_os = "windows")]
    {
        let _ = mac;
        windows
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = windows;
        mac
    }
}

/// A large file a game needs that must NOT come down again with every update.
///
/// The case this exists for is a bundled language model: hundreds of megabytes
/// that change maybe never, sitting next to a game bundle that changes daily.
/// Putting it inside the game archive would mean re-downloading all of it for
/// every build, and `install` wipes the game folder anyway — so a payload
/// lands OUTSIDE that folder, beside the game's own saves, and is fetched only
/// when it is missing.
struct Payload {
    /// The release TAG the payload lives on — a fixed, standalone release,
    /// separate from the game's versioned ones, so a gigabyte of weights is
    /// uploaded once rather than attached to every build. `check_latest`
    /// never sees it because it carries no game bundle.
    tag: &'static str,
    /// The release-asset SUFFIXES to fetch, matched the same way bundles are.
    assets: &'static [&'static str],
    /// The game's support-directory name. Must match what the game itself
    /// computes for its saves, because that is how the game finds this file:
    /// no handshake, no environment variable, one shared convention. (macOS
    /// `open` does not pass environment through to a bundle, so a handshake
    /// was never really on the table.)
    support_dir: &'static str,
    /// The subfolder within it.
    into: &'static str,
}

/// What a card is: something to play, or something to work in. The only
/// difference is the word on the button and where it sits on the shelf -
/// a tool is still installed and updated exactly like a game.
#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Game,
    Tool,
}

struct Game {
    slug: &'static str,        // stable id (folder name, UI key)
    name: &'static str,        // display name
    tagline: &'static str,     // one-line blurb on the card
    repo: &'static str,        // owner/name on GitHub (must be public)
    accent: &'static str,      // brand colour (hex) — drives the card's gradient / glow in the UI
    delivery: Delivery,        // web bundle vs native build
    kind: Kind,                // a game to play, or a tool to work in
    /// A big companion file kept out of the update cycle. `None` for almost
    /// every game.
    payload: Option<Payload>,
}

const GAMES: &[Game] = &[
    Game {
        slug: "wriftheart",
        name: "WriftHeart",
        tagline: "An 8-bit action-RPG of a shattered world. Gather the ten shards; mend the Wriftheart.",
        repo: "Baz-Studios-LLC/wriftheart",
        accent: "#b06cff",
        // The Rust + Bevy rewrite ships as a native build per platform. Asset SUFFIXES (matched by
        // `bundle_url`'s ends_with) — the studio native convention, so versioned names still match.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "wingman",
        name: "Wingman",
        tagline: "A twin-stick shooter where you fly two ships at once.",
        repo: "Baz-Studios-LLC/Wingman",
        accent: "#3a86ff",
        delivery: Delivery::Web { asset: "wingman-game.zip", port: 47824 },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "violet-edge",
        name: "VIOLET EDGE",
        tagline: "A neon-vector Asteroids love letter — cut the field, hold the edge.",
        repo: "Baz-Studios-LLC/Violet-Edge",
        accent: "#8a5cff",
        // Rust + Bevy native build per platform (same delivery as WriftHeart). Renamed from the old
        // web "Neon Edge"/Neon-Drift entry — that was the retired JS build. Asset SUFFIXES (ends_with),
        // so the game can version its asset names without breaking this.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "divus-factus",
        name: "Divus Factus",
        tagline: "A god game where the villagers' belief defines the god.",
        // Renamed from Egregore (a 2024 Steam game took the name). The GitHub
        // repo still carries the old name; GitHub's API redirects a renamed
        // repo, so this keeps working whichever it ends up being — but the
        // slug changed, which retires the old install folder below.
        repo: "Baz-Studios-LLC/egregore",
        accent: "#d4a24e",
        // Rust + Bevy native build per platform, same delivery as WriftHeart.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        // The villagers' own words: a language model the game loads from
        // beside its saves. The weights and their tokenizer live on the
        // fixed `models-1` release; the game runs fine without them, it
        // just keeps to its written lines.
        kind: Kind::Game,
        payload: Some(Payload {
            tag: "models-1",
            // Just the weights: the tokenizer lives inside the GGUF since
            // the game moved to llama.cpp.
            assets: &[".gguf"],
            support_dir: "Divus Factus",
            into: "models",
        }),
    },
    Game {
        slug: "crashout",
        name: "Crashout",
        tagline: "An isometric office-survival game — hold it together before you crash out.",
        repo: "Baz-Studios-LLC/Crashout",
        accent: "#ff5a34",
        delivery: Delivery::Web { asset: "crashout-game.zip", port: 47826 },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "please-dont-shake",
        name: "Please Don't Shake",
        tagline: "An ant farm that digs its own tunnels — and one sign asking you not to shake it.",
        // Private for now, so this reads "Coming soon" until it goes public and a
        // release carries the native build. The launcher downloads unauthenticated.
        repo: "Baz-Studios-LLC/please-dont-shake",
        // The red marker on the sticker the game is named after.
        accent: "#ca1e1b",
        // Rust + Bevy native build per platform, same delivery as WriftHeart.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "fly-on-the-wall",
        name: "Fly on the Wall",
        tagline: "A housefly in a family home. You can't understand a word they say \u{2014} only watch.",
        repo: "Baz-Studios-LLC/Fly-on-the-Wall",
        // A housefly is not black: it is dark slate with a green sheen, and the
        // greens were the only part of the shelf nobody had taken.
        accent: "#4f9d8a",
        // Rust + Bevy native build per platform, same delivery as WriftHeart.
        // Its bundle carries an assets folder beside the binary, which changes
        // nothing here \u{2014} the launcher installs the whole .app either way.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "ranger",
        name: "Ranger",
        tagline: "Raise monsters on a ranch, cross a continent, earn your Ranger License.",
        repo: "Baz-Studios-LLC/ranger-game",
        // Ranger green, and the only green left once the housefly took the
        // teal - this one is the field rather than the sheen.
        accent: "#5faa5c",
        // Rust + Bevy native build per platform, same delivery as the rest.
        // Its bundle carries an assets folder beside the binary because the
        // world is read from it at runtime; the launcher installs the whole
        // thing either way, so that changes nothing here.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        kind: Kind::Game,
        payload: None,
    },
    Game {
        slug: "opificium",
        name: "Opificium",
        tagline: "The maker's bench: draw a building by hand, pose a body, export both as files.",
        repo: "Baz-Studios-LLC/Opificium",
        // Workshop brass against all the games' colours: it should read as
        // the odd one out on the shelf, because it is.
        accent: "#b8863b",
        // Rust + Bevy native build per platform, same delivery as the games.
        delivery: Delivery::Native {
            mac: "-macos-aarch64.app.tar.gz",
            windows: "-windows-x86_64.zip",
        },
        // A tool, not a game: it opens a project rather than a world, and
        // it carries no game's content - the work lives in whichever
        // game's repository it belongs to.
        kind: Kind::Tool,
        payload: None,
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
    /// "game" or "tool" - the UI says Play for one and Open for the other.
    kind: String,
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
            kind: match g.kind {
                Kind::Game => "game".to_string(),
                Kind::Tool => "tool".to_string(),
            },
            repo: g.repo.to_string(),
            accent: g.accent.to_string(),
        })
        .collect()
}

/// Install folders left behind by games that have since been renamed.
///
/// A game is keyed by its slug and a slug IS its folder name, so renaming one
/// orphans gigabytes in the old folder with nothing left to launch it. Listed
/// by hand rather than swept generically: deleting every folder we do not
/// recognise is how a launcher eats something it should not have.
const RETIRED_SLUGS: &[&str] = &[
    // Egregore became Divus Factus.
    "egregore",
];

/// Deletes retired install folders. Runs once, at startup, and is silent when
/// there is nothing to clear — which is every launch after the first.
fn sweep_retired(app: &tauri::AppHandle) {
    let Ok(base) = app.path().app_data_dir() else {
        return;
    };
    let games = base.join("games");
    for slug in RETIRED_SLUGS {
        // Never touch a folder a live game still answers to.
        if GAMES.iter().any(|game| game.slug == *slug) {
            continue;
        }
        let dir = games.join(slug);
        if !dir.exists() {
            continue;
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => eprintln!("cleared the retired install at {}", dir.display()),
            Err(e) => eprintln!("could not clear {}: {e}", dir.display()),
        }
    }
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
    let game = game_by_slug(&slug)?;
    let dir = game_dir(&app, &slug).ok()?;
    // A version stamp alone isn't "installed" — the delivery's actual artifact must be present.
    // This also HEALS a leftover install of a different delivery: e.g. the old web bundle for a
    // game that has since gone native (no .app) reads as not-installed, so the UI offers Install
    // (a fresh install wipes the stale folder) instead of a Play that can't find an executable.
    let present = match &game.delivery {
        Delivery::Web { .. } => dir.join("index.html").exists(),
        Delivery::Native { .. } => native_artifact(&dir),
    };
    if !present {
        return None;
    }
    fs::read_to_string(dir.join("version.txt"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Is a native game's launchable artifact present in `dir` (the .app on macOS, an .exe on Windows)?
fn native_artifact(dir: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        find_entry(dir, "app").is_some()
    }
    #[cfg(target_os = "windows")]
    {
        find_entry(dir, "exe").is_some()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = dir;
        false
    }
}

// Pull the download URL for a game's bundle asset out of a release's asset list, if present.
// Matches by SUFFIX, not exact name, so a game can version or rename its asset
// (e.g. `violet-edge-v0.3.0-windows-x86_64.zip`) without breaking the launcher — the catalog stores a
// stable tail like `-windows-x86_64.zip`. Within one repo's release there's only one asset per suffix.
fn bundle_url(release: &serde_json::Value, asset: &str) -> Option<String> {
    release["assets"].as_array()?.iter().find_map(|a| {
        let name = a["name"].as_str()?;
        if name.ends_with(asset) {
            a["browser_download_url"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Where a game keeps its own files — saves, and any payload we fetch for it.
///
/// This MIRRORS what the game computes for itself. It is duplicated rather than
/// negotiated because macOS `open` hands a bundle to LaunchServices and drops
/// the environment on the way, so there is no clean channel to tell a game
/// where we put something. One shared convention instead, and if a game ever
/// changes its support directory this must change with it.
fn support_path(support_dir: &str, into: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support")
                .join(support_dir)
                .join(into),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(PathBuf::from(appdata).join(support_dir).join(into))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = std::env::var("HOME").ok()?;
        // Matches the game's XDG-ish fallback: lower-cased, hyphenated.
        Some(
            PathBuf::from(home)
                .join(".local/share")
                .join(support_dir.to_lowercase().replace(' ', "-"))
                .join(into),
        )
    }
}

/// Fetches a game's payload, if it declares one and does not already have it.
///
/// Skipped whenever the file is already there at the size the release says it
/// should be — which is every install after the first. A partial or truncated
/// download (a closed lid mid-fetch) fails the size check and is fetched again
/// rather than left to be loaded as a corrupt model.
async fn fetch_payload(game: &Game) -> Result<(), String> {
    let Some(payload) = &game.payload else {
        return Ok(());
    };
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .map_err(|e| e.to_string())?;
    let api = format!(
        "https://api.github.com/repos/{}/releases/tags/{}",
        game.repo, payload.tag
    );
    let resp = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        // No payload release published yet. Not an error: the game is built
        // to run without it.
        eprintln!("no payload release {} for {}", payload.tag, game.slug);
        return Ok(());
    }
    let release: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

    let Some(dir) = support_path(payload.support_dir, payload.into) else {
        return Err("could not find the game's support directory".into());
    };
    for suffix in payload.assets {
        let Some((url, name, size)) = payload_asset(&release, suffix) else {
            eprintln!("payload release carries nothing ending {suffix}");
            continue;
        };
        let dest = dir.join(&name);
        if let Ok(meta) = fs::metadata(&dest) {
            if size == 0 || meta.len() == size {
                continue;
            }
        }
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("payload download returned {}", resp.status()));
        }
        // Streamed to disk a chunk at a time: a model is a gigabyte, and a
        // gigabyte does not belong in a tester's RAM on the way through.
        // Written beside the target and renamed, so an interrupted fetch can
        // never leave a half-file sitting where a whole one belongs.
        let part = dir.join(format!("{name}.part"));
        let mut file = fs::File::create(&part).map_err(|e| e.to_string())?;
        let mut written: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            use std::io::Write;
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            written += chunk.len() as u64;
        }
        drop(file);
        if size != 0 && written != size {
            let _ = fs::remove_file(&part);
            return Err(format!(
                "payload {name} arrived {written} bytes of {size}"
            ));
        }
        fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        eprintln!("fetched {} ({written} bytes)", dest.display());
    }
    Ok(())
}

/// A payload asset's URL, filename and expected size, matched by suffix.
fn payload_asset(release: &serde_json::Value, asset: &str) -> Option<(String, String, u64)> {
    release["assets"].as_array()?.iter().find_map(|a| {
        let name = a["name"].as_str()?;
        if !name.ends_with(asset) {
            return None;
        }
        let url = a["browser_download_url"].as_str()?.to_string();
        let size = a["size"].as_u64().unwrap_or(0);
        Some((url, name.to_string(), size))
    })
}

/// Find a game's newest release: scan its repo's release list (newest first) and take the first
/// published (non-draft, non-prerelease) release that actually carries its game bundle. Skips the
/// installer-only / launcher releases, so they never masquerade as "latest game".
#[tauri::command]
async fn check_latest(slug: String) -> Result<Latest, String> {
    let game = game_by_slug(&slug).ok_or("unknown game")?;
    let rel = latest_release(game).await?;
    let version = rel["tag_name"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    let url = bundle_url(&rel, game.delivery.asset()).ok_or("no published game release found")?;
    let notes = rel["body"].as_str().unwrap_or("").to_string();
    Ok(Latest { version, url, notes })
}

/// The newest published release that actually carries this game's bundle.
///
/// Shared by the version check and by the payload fetch, so both are looking at
/// the same release rather than two separate answers to the same question.
async fn latest_release(game: &Game) -> Result<serde_json::Value, String> {
    // ANSWERED FROM MEMORY IF IT WAS ASKED RECENTLY.
    //
    // GitHub allows SIXTY unauthenticated calls an hour PER IP, and this
    // launcher spends them freely: every game checks on open, on every
    // refresh, and again around an install. Brett hit the wall with a
    // launcher stuck three versions behind and no way to tell - the API was
    // answering 403 to every call it made, and the screen looked exactly like
    // being up to date.
    //
    // A few minutes of memory is the difference between a handful of calls an
    // hour and one per click.
    if let Some(remembered) = remembered_release(game.repo) {
        return Ok(remembered);
    }
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
    // THE ONE FAILURE WORTH NAMING. Anything else is a network having a bad
    // day; this one is a wall that lasts an hour, is invisible from the
    // outside, and is the likeliest thing to be wrong when a launcher will not
    // update. The message travels all the way to the screen.
    if resp.status() == reqwest::StatusCode::FORBIDDEN
        || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        let spent = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "0");
        if spent {
            return Err(RATE_LIMITED.to_string());
        }
        return Err(format!("GitHub returned {}", resp.status()));
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub returned {}", resp.status()));
    }
    let releases: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let list = releases.as_array().ok_or("unexpected GitHub response")?;
    for rel in list {
        if rel["draft"].as_bool() == Some(true) || rel["prerelease"].as_bool() == Some(true) {
            continue;
        }
        if bundle_url(rel, game.delivery.asset()).is_some() {
            if rel["tag_name"].as_str().unwrap_or("").is_empty() {
                continue;
            }
            remember_release(game.repo, rel);
            return Ok(rel.clone());
        }
    }
    Err("no published game release found".into())
}

/// What the launcher says when GitHub has stopped answering it.
///
/// A sentence rather than a code, because it goes on the screen and the person
/// reading it needs to know that nothing is broken and that waiting fixes it.
const RATE_LIMITED: &str =
    "GitHub is rate-limiting this machine (60 checks an hour). Version unknown until it clears — usually within the hour.";

/// How long a release answer is worth trusting.
///
/// Long enough that opening the launcher, clicking about and installing
/// something is ONE call rather than a dozen; short enough that a release
/// pushed while the launcher is open is seen without a restart.
const REMEMBER_FOR: Duration = Duration::from_secs(300);

/// The last answer GitHub gave about each repo, and when.
static REMEMBERED: Mutex<Option<HashMap<&'static str, (Instant, serde_json::Value)>>> =
    Mutex::new(None);

fn remembered_release(repo: &'static str) -> Option<serde_json::Value> {
    let guard = REMEMBERED.lock().ok()?;
    let (when, what) = guard.as_ref()?.get(repo)?;
    (when.elapsed() < REMEMBER_FOR).then(|| what.clone())
}

fn remember_release(repo: &'static str, rel: &serde_json::Value) {
    if let Ok(mut guard) = REMEMBERED.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(repo, (Instant::now(), rel.clone()));
    }
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
    // A declared payload is fetched FIRST, and only if missing. Before the
    // bundle rather than after, so a game is never briefly installed without
    // the companion file it expects — and skipped outright on every update
    // after the first, which is the whole point of keeping it out here.
    if let Some(game) = game_by_slug(&slug) {
        if game.payload.is_some() {
            // A missing or unreachable payload is not a reason to refuse an
            // install: the game is built to run without it.
            if let Err(e) = fetch_payload(game).await {
                eprintln!("could not fetch the payload: {e}");
            }
        }
    }
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("download returned {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;

    // Web bundles + Windows native builds ship as .zip; macOS native builds ship as a
    // .app packed in .tar.gz (a zip mangles the bundle's symlinks + exec bits).
    let is_tar = url.ends_with(".tar.gz") || url.ends_with(".tgz");
    let dir2 = dir.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        if dir2.exists() {
            fs::remove_dir_all(&dir2).map_err(|e| e.to_string())?;
        }
        fs::create_dir_all(&dir2).map_err(|e| e.to_string())?;
        if is_tar {
            // Unpack via the system `tar` (present on macOS, the only place tar assets land) —
            // it preserves the .app's exec bits and structure that a Rust zip reader wouldn't.
            let tmp = dir2.join("__download.tar.gz");
            fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
            let status = Command::new("tar")
                .arg("-xzf")
                .arg(&tmp)
                .arg("-C")
                .arg(&dir2)
                .status()
                .map_err(|e| e.to_string())?;
            let _ = fs::remove_file(&tmp);
            if !status.success() {
                return Err("failed to unpack the game archive".into());
            }
        } else {
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
            archive.extract(&dir2).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;

    // A build we downloaded ourselves usually isn't quarantined, but strip it defensively so
    // macOS Gatekeeper never blocks the (ad-hoc-signed) game the first time it's launched.
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("xattr")
            .arg("-dr")
            .arg("com.apple.quarantine")
            .arg(&dir)
            .status();
    }

    fs::write(dir.join("version.txt"), version).map_err(|e| e.to_string())?;
    Ok(())
}

/// Play a game. A NATIVE game is spawned as its OWN process — it runs in its own window and this
/// window stays the library. A WEB game reuses this window: we reconfigure it (bigger, decorated,
/// resizable) and navigate it to that game's local server. (We deliberately never open a second
/// WebviewWindow — on some Windows/WebView2 setups an extra window never paints.) Each web game is
/// served over its own fixed localhost port (a stable origin, so saves persist), started lazily.
#[tauri::command]
fn play(app: tauri::AppHandle, slug: String, serving: State<Serving>) -> Result<(), String> {
    let game = game_by_slug(&slug).ok_or("unknown game")?;
    let dir = game_dir(&app, &slug)?;

    // Native: launch the installed executable and leave this window on the library.
    let port = match &game.delivery {
        Delivery::Native { .. } => return launch_native(&dir),
        Delivery::Web { port, .. } => *port,
    };

    // Web: serve the bundle and show it in this window.
    if !dir.join("index.html").exists() {
        return Err("that game isn't installed yet".into());
    }
    {
        let mut bound = serving.0.lock().map_err(|_| "server lock poisoned".to_string())?;
        if !bound.contains(&port) {
            let pads = pad_state(&serving);
            serve(dir.clone(), port, pads, app.clone())?;
            bound.insert(port);
        }
    }
    let url: tauri::Url = format!("http://127.0.0.1:{}/", port)
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

/// Spawn a NATIVE game as its own detached process: macOS `open`s the .app bundle; Windows runs
/// the .exe. The launcher does not wait on it — the game owns its own window and lifetime.
fn launch_native(dir: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let app = find_entry(dir, "app").ok_or("that game isn't installed yet")?;
        Command::new("open").arg(&app).spawn().map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let exe = find_entry(dir, "exe").ok_or("that game isn't installed yet")?;
        Command::new(&exe).current_dir(dir).spawn().map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = dir;
        Err("native games aren't supported on this platform".into())
    }
}

/// The first entry in `dir` whose name ends in `.<ext>` — the installed `.app` bundle (macOS) or
/// the game `.exe` (Windows).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn find_entry(dir: &Path, ext: &str) -> Option<PathBuf> {
    let want = format!(".{ext}");
    fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.file_name().and_then(|n| n.to_str()).map(|n| n.ends_with(&want)).unwrap_or(false)
    })
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
        .setup(|app| {
            // A renamed game leaves its old install behind; clear it before
            // the library is drawn, so a tester never sees two of one game.
            sweep_retired(app.handle());
            Ok(())
        })
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

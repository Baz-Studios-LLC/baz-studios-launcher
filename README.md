# Baz Studios Launcher

One hub that keeps every Baz Studios game — and itself — up to date, and runs them. A single
polished window with a card per game: install, update, and play, with each game's own art and
brand colour.

Each Baz Studios game ships in one of two forms, and the launcher handles both:

- **Web bundle** — a pure-web game (`index.html` + `js/`, all art baked into JS). The launcher
  *downloads and serves* its static files over a fixed localhost port and opens it in this window.
  The game knows nothing about the launcher — the downloaded files still run in any browser/webview.
- **Native build** — a real platform executable (e.g. the Rust + Bevy WriftHeart). The launcher
  downloads the build for your OS, installs it, and *launches it as its own process* in its own
  window; the launcher stays on the library.

## What it does

1. On start, it self-updates (checks GitHub for a newer launcher; if found, downloads + verifies +
   swaps it in and relaunches — the last time you touch macOS Gatekeeper).
2. For each game in the catalog, it asks that game's GitHub repo for the newest **published**
   release carrying that game's asset (a web bundle, or the native build for your platform), and
   compares it to the locally-installed copy.
3. **Install / Update** downloads the asset and unpacks it into the OS app-data dir (de-quarantining
   native macOS builds so Gatekeeper doesn't block them).
4. **Play** runs the game — a web bundle is served over its fixed localhost port and shown in this
   window (a stable `localStorage` origin, so saves persist); a native build is spawned as its own
   process. A web game's in-page "Exit" returns you to the library.

A game whose repo has no such published release (private, or no `*-game.zip` yet) simply shows
**Coming soon** — the launcher degrades gracefully and never errors.

## The catalog

Games are **baked into the launcher** — add one by adding a row to `GAMES` in
`src-tauri/src/main.rs` (and dropping optional cover art at `src/assets/<slug>.png`):

| Field | Meaning |
| --- | --- |
| `slug` | stable id — the app-data folder name and the UI key |
| `name` / `tagline` | shown on the card |
| `repo` | `owner/name` on GitHub — **must be public** (the launcher downloads unauthenticated) |
| `asset` | the release asset it downloads (the game's web bundle, zipped) |
| `port` | fixed localhost port — a stable, per-game save origin |
| `accent` | brand colour (hex) — drives the card's gradient / glow |

Optional per-game art: `src/assets/<slug>.png`. A wide image (aspect ≥ 2) is treated as a wordmark;
a squarer one as an icon. No file → a typographic monogram fallback.

### For a game to become playable

Its repo must be **public** and its release CI must attach a `<slug>-game.zip` — a zip of the game's
web bundle (`index.html` + `js/` + assets). WriftHeart already does this; the others need the same
one-line CI step to light up (until then they read "Coming soon").

## Build / develop

Prereqs: Rust (stable) + the [Tauri v2 system deps](https://v2.tauri.app/start/prerequisites/) + Node 20.

```bash
npm install
npm run dev      # run the launcher locally
npm run build    # produce installers for the current OS
```

Preview the UI in a plain browser (no Tauri needed) — it falls back to a mock backend:

```bash
cd src && python3 -m http.server 8137   # then open http://localhost:8137
```

> The Rust backend (`src-tauri/src/main.rs`) can only be verified by an actual `cargo`/Tauri build.

## Release

Installers are built + published on demand via `.github/workflows/release.yml`
(Actions → Release → Run workflow → e.g. `v0.1.0`). The job builds macOS + Windows, publishes the
release, then commits the signed `updater.json` to `main` — the stable endpoint the installed
launcher polls to keep itself current.

**Secrets** the workflow needs (org- or repo-level): `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (the minisign keypair whose public half is baked into
`tauri.conf.json`). Reuse the existing Baz Studios keypair so the pubkey already matches.

## Layout

```
baz-studios-launcher/
  src/
    index.html              the library UI (self-contained; previews in a browser via the mock)
    assets/<slug>.png       per-game cover art
  src-tauri/
    src/main.rs             catalog + check/download/unpack/serve/play, gamepad bridge, self-update
    tauri.conf.json         decorated 1040×640 window, updater endpoint, points frontendDist at ../src
    capabilities/default.json
    Cargo.toml, build.rs, icons/
  .github/workflows/release.yml
  package.json
  updater.json              (committed by CI after the first release)
```

# Steam Transfer

Back up your installed Steam games to an external drive, then restore them on another PC
without downloading them again. It looks like an old CD wallet: every game is a disc in a sleeve.

Built with Rust and [Tauri 2](https://tauri.app). The installer is a few MB and needs no admin rights.

## What it does

| | |
|---|---|
| **Back up** | Copies a game's files, its Steam manifest and any save folders it can find to `<drive>\Steam Transfer\<Game> (App <id>)`. |
| **Resume** | If a copy is stopped or fails (cable pulled, PC slept), start it again: finished files are kept and only the rest is copied. Every file is written to a `.steamtransfer-part` file first and renamed when it's complete, so a half-copied file never looks finished. |
| **Update** | When Steam updates a game, its disc shows **UPDATE**. Updating copies only the changed files and removes files the update deleted. The same goes for restoring: an older install on the other PC is brought up to the drive's build. |
| **Restore** | Pick the drive and a Steam library. The game files and manifest go into the library, so Steam sees the game as installed. Steam has to be closed while this runs. |
| **Verify** | Every copy is checked file by file (exact sizes, file count, total bytes). "Check every byte" compares the backup with the installed game byte for byte. |
| **Queue** | Pick several games and they copy one after another (one at a time is fastest on a USB drive). |
| **Self-update** | On startup the app checks this repo's latest GitHub Release and offers to install it. |

Backups made with Steam Transfer 1.x (the C# version) are picked up as they are.

## Install

Download `Steam Transfer_x.y.z_x64-setup.exe` from the [latest release](../../releases/latest) and run it.

## Develop

Requires Rust (stable), Node 18+ and the WebView2 runtime (already on Windows 10/11).

```powershell
npm install
npm run dev          # run the app with hot reload of ui/
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run build        # release build + installer in src-tauri/target/release/bundle/
```

To preview the UI with sample data in a normal browser, serve the `ui/` folder
(`python -m http.server --directory ui`). `ui/mock.js` stands in for the Rust side there.

### Website libraries

Open **Download**, choose **Add** beside **Website**, and enter an authorized site's main URL. This
stores the source without importing its entire catalog. Select that source, type a title in the
website search box, and press **Search**. Steam Transfer requests `?s=<your query>`, chooses the first
same-site result whose visible title contains the query, opens that result page, and adds its first
magnet plus artwork to the library. Magnet cards are handed to the built-in torrent transfer engine.

Magnet detection checks ordinary `href` links as well as magnet-labeled buttons and images. It
reads visible text, image `alt` text, ARIA/title labels, `data-magnet`, `data-url`, `data-href`, and
`onclick` values, including URL-encoded magnet links. Generic labels such as “Magnet” use the
magnet's `dn=` name or the child page title so unrelated items do not collapse as duplicates.

Duplicate titles are shown once (case, spacing, and punctuation differences are ignored), and
artwork from a duplicate can fill a missing cover on the kept item. Search results never follow a
link to a different website.

The generic importer supports ordinary server-rendered HTML. JavaScript-only catalogs, sign-in
flows, and site-specific buttons may require a dedicated provider.

### Layout

```
ui/                   index.html, styles.css, app.js (no framework, no build step)
src-tauri/src/
  steam.rs            find Steam, libraries, installed games, cover art
  vdf.rs              reader for Valve's .acf / .vdf files
  engine.rs           copy / resume / mirror / verify engine (CopyFileExW, long paths)
  package.rs          backup folder format ("binder")
  jobs.rs             transfer queue and the backup / restore / verify jobs
  lib.rs              Tauri commands, events, cover-art protocol, updater
```

## Release a new version

Everything happens on this PC (about 2 minutes):

```powershell
node scripts/release.mjs 2.0.2 "What changed in this version"
```

This bumps the version, runs the tests, builds and signs the installer, commits, tags, pushes,
and uploads the installer, its signature and `latest.json` as a GitHub Release. Installed copies
offer the update the next time they start.

It needs the updater signing key at `%USERPROFILE%\.tauri\steam-transfer.key` and `gh` signed in
to the account that owns the repo. Never commit the key. If it's lost, installed copies can't verify
new updates, so keep a copy somewhere safe.

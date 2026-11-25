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

1. `node scripts/bump.mjs 2.0.1`
2. `git commit -am "v2.0.1"`, then `git tag v2.0.1`, then `git push --follow-tags`

The **Release** workflow builds the installer, signs the update and publishes the GitHub Release
with `latest.json`. Installed copies pick it up the next time they start.

The workflow needs two repository secrets: `TAURI_SIGNING_PRIVATE_KEY` (the contents of the
private key file) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The private key must never be committed.
If it's lost, installed copies can no longer verify updates, so keep a copy somewhere safe.

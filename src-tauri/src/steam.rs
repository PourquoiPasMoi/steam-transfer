//! Finding Steam, its library folders, and installed games.

use crate::vdf;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// "Steamworks Common Redistributables" - not a game, never worth copying.
const SKIP_APPS: &[&str] = &["228980"];

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub app_id: String,
    pub name: String,
    pub install_dir: String,
    pub game_path: PathBuf,
    pub manifest_path: PathBuf,
    pub library_root: PathBuf,
    pub size_on_disk: u64,
    pub build_id: String,
    pub state_flags: u32,
    pub last_updated: u64,
    #[serde(skip)]
    pub art: Option<PathBuf>,
}

impl Game {
    /// Steam's StateFlags bit 4 = fully installed; anything else means an update/download is pending.
    pub fn fully_installed(&self) -> bool {
        self.state_flags == 4 || self.state_flags == 0
    }
}

pub fn steam_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        use winreg::RegKey;
        let candidates = [
            (HKEY_CURRENT_USER, "Software\\Valve\\Steam", "SteamPath"),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\WOW6432Node\\Valve\\Steam", "InstallPath"),
            (HKEY_LOCAL_MACHINE, "SOFTWARE\\Valve\\Steam", "InstallPath"),
        ];
        for (hive, key, value) in candidates {
            if let Ok(k) = RegKey::predef(hive).open_subkey(key) {
                if let Ok(path) = k.get_value::<String, _>(value) {
                    let path = PathBuf::from(path.replace('/', "\\"));
                    if path.is_dir() {
                        return Some(path);
                    }
                }
            }
        }
    }
    let default = PathBuf::from("C:\\Program Files (x86)\\Steam");
    default.is_dir().then_some(default)
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

/// Every Steam library folder on this PC (folders that contain `steamapps`).
pub fn libraries() -> Vec<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    let Some(root) = steam_root() else { return libs };
    libs.push(root.clone());
    if let Ok(text) = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        for path in vdf::values(&text, "path") {
            let path = PathBuf::from(path);
            if !libs.iter().any(|l| same_path(l, &path)) {
                libs.push(path);
            }
        }
    }
    libs.retain(|l| l.join("steamapps").is_dir());
    libs
}

/// Turn a folder the user picked (drive root, library, steamapps, ...) into a Steam library root.
pub fn detect_library(selected: &Path) -> Option<PathBuf> {
    if !selected.is_dir() {
        return None;
    }
    let mut full = selected.to_path_buf();
    if full.file_name().map(|n| n.eq_ignore_ascii_case("steamapps")).unwrap_or(false) {
        full.pop();
    }
    if full.join("steamapps").is_dir() {
        return Some(full);
    }
    // A known library on the same drive?
    let drive = full.components().next().map(|c| c.as_os_str().to_string_lossy().to_uppercase());
    if let Some(known) = libraries().into_iter().find(|l| {
        l.components().next().map(|c| c.as_os_str().to_string_lossy().to_uppercase()) == drive
    }) {
        return Some(known);
    }
    ["SteamLibrary", "Steam", "Program Files (x86)\\Steam"]
        .iter()
        .map(|c| full.join(c))
        .find(|c| c.join("steamapps").is_dir())
}

pub fn read_manifest(manifest: &Path, library: &Path) -> Option<Game> {
    let text = fs::read_to_string(manifest).ok()?;
    let app_id = vdf::value(&text, "appid")?;
    let name = vdf::value(&text, "name")?;
    let install_dir = vdf::value(&text, "installdir")?;
    if app_id.is_empty() || name.is_empty() || install_dir.is_empty() {
        return None;
    }
    let num = |key: &str| vdf::value(&text, key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    Some(Game {
        game_path: library.join("steamapps").join("common").join(&install_dir),
        manifest_path: manifest.to_path_buf(),
        library_root: library.to_path_buf(),
        size_on_disk: num("SizeOnDisk"),
        build_id: vdf::value(&text, "buildid").unwrap_or_default(),
        state_flags: num("StateFlags") as u32,
        last_updated: num("LastUpdated"),
        art: None,
        app_id,
        name,
        install_dir,
    })
}

/// Installed games in one library (only those whose game folder actually exists).
pub fn scan_library(library: &Path) -> Vec<Game> {
    let steamapps = library.join("steamapps");
    let Ok(entries) = fs::read_dir(&steamapps) else { return Vec::new() };
    let mut games = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let lower = file_name.to_ascii_lowercase();
        if !(lower.starts_with("appmanifest_") && lower.ends_with(".acf")) {
            continue;
        }
        if let Some(game) = read_manifest(&entry.path(), library) {
            if SKIP_APPS.contains(&game.app_id.as_str()) || !game.game_path.is_dir() {
                continue;
            }
            games.push(game);
        }
    }
    games
}

pub fn scan_all() -> Vec<Game> {
    let root = steam_root();
    let mut games: Vec<Game> = Vec::new();
    for lib in libraries() {
        for mut game in scan_library(&lib) {
            if games.iter().any(|g| g.app_id == game.app_id) {
                continue;
            }
            game.art = root.as_deref().and_then(|r| find_art(r, &game.app_id));
            games.push(game);
        }
    }
    games.sort_by_key(|g| g.name.to_lowercase());
    games
}

/// Portrait cover art from Steam's local cache, if present.
pub fn find_art(steam_root: &Path, app_id: &str) -> Option<PathBuf> {
    let cache = steam_root.join("appcache").join("librarycache");
    let folder = cache.join(app_id);
    let names = ["library_600x900.jpg", "library_600x900_2x.jpg", "library_600x900.png"];
    for name in names {
        let p = folder.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // Newer Steam builds nest assets one level deeper: librarycache/<id>/<hash>/library_600x900.jpg
    if let Ok(entries) = fs::read_dir(&folder) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                for name in names {
                    let p = entry.path().join(name);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    let legacy = cache.join(format!("{app_id}_library_600x900.jpg"));
    if legacy.is_file() {
        return Some(legacy);
    }
    // Cover cache written by Steam Transfer 1.x
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("SteamTransfer").join("covers").join(format!("{app_id}.jpg"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Steam Cloud / local save folders: <steam>/userdata/<account>/<appid>
pub fn userdata_dirs(app_id: &str) -> Vec<(String, PathBuf)> {
    let Some(root) = steam_root() else { return Vec::new() };
    let Ok(entries) = fs::read_dir(root.join("userdata")) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path().join(app_id);
            dir.is_dir().then(|| (e.file_name().to_string_lossy().to_string(), dir))
        })
        .collect()
}

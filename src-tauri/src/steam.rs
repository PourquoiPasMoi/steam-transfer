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
                        return Some(true_case(path));
                    }
                }
            }
        }
    }
    let default = PathBuf::from("C:\\Program Files (x86)\\Steam");
    default.is_dir().then_some(default)
}

/// Steam stores its path in lowercase ("c:/program files (x86)/steam"); ask Windows for the real capitalisation.
fn true_case(path: PathBuf) -> PathBuf {
    match fs::canonicalize(&path) {
        Ok(full) => {
            let s = full.to_string_lossy();
            match s.strip_prefix(r"\\?\") {
                // keep plain drive paths only; leave UNC/odd forms as they were
                Some(rest) if rest.chars().nth(1) == Some(':') => PathBuf::from(rest),
                _ => path,
            }
        }
        Err(_) => path,
    }
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

// ---------------------------------------------------------------------------------------------
// Libraries on every drive, and creating / registering new ones

#[derive(Debug, Clone, Serialize)]
pub struct LibraryInfo {
    pub path: PathBuf,
    /// listed in Steam's libraryfolders.vdf (Steam knows about it)
    pub registered: bool,
}

/// A folder that is a Steam library: it has `steamapps` with a library marker, games or a
/// `common` folder. Our own backup packages also contain `steamapps`, so they are excluded.
fn looks_like_library(dir: &Path) -> bool {
    let apps = dir.join("steamapps");
    if !apps.is_dir() || dir.join(crate::package::STATUS_FILE).exists() || dir.join(crate::package::META_FILE).exists() {
        return false;
    }
    if apps.join("libraryfolder.vdf").is_file() || apps.join("libraryfolders.vdf").is_file() || apps.join("common").is_dir() {
        return true;
    }
    fs::read_dir(&apps)
        .map(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("appmanifest_")))
        .unwrap_or(false)
}

/// Libraries found by looking on every drive: the drive root, its top-level folders, and one level
/// deeper inside folders whose names suggest games or Steam (e.g. D:\Games\SteamLibrary).
pub fn discover_libraries() -> Vec<PathBuf> {
    const SKIP: &[&str] = &["windows", "$recycle.bin", "system volume information", "programdata", "recovery", "perflogs", "users", "msocache", "config.msi", "$windows.~bt", "$windows.~ws"];
    let binder = crate::package::BINDER_DIR.to_lowercase();
    let mut found = Vec::new();
    for root in crate::sys::drive_roots() {
        if looks_like_library(&root) {
            found.push(root.clone());
        }
        let Ok(entries) = fs::read_dir(&root) else { continue };
        for entry in entries.flatten().take(500) {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !dir.is_dir() || SKIP.contains(&name.as_str()) || name == binder {
                continue;
            }
            if looks_like_library(&dir) {
                found.push(dir.clone());
                continue;
            }
            let worth_a_look = name.contains("steam") || name.contains("game") || name.starts_with("program files");
            if !worth_a_look {
                continue;
            }
            let Ok(subs) = fs::read_dir(&dir) else { continue };
            for sub in subs.flatten().take(500) {
                let sub_path = sub.path();
                if sub_path.is_dir() && sub.file_name().to_string_lossy().to_lowercase() != binder && looks_like_library(&sub_path) {
                    found.push(sub_path);
                }
            }
        }
    }
    found
}

/// Every Steam library on this PC: the ones Steam knows about first, then any found on the drives.
pub fn all_libraries() -> Vec<LibraryInfo> {
    let mut libs: Vec<LibraryInfo> = libraries().into_iter().map(|path| LibraryInfo { path, registered: true }).collect();
    for path in discover_libraries() {
        if !libs.iter().any(|l| same_path(&l.path, &path)) {
            libs.push(LibraryInfo { path: true_case(path), registered: false });
        }
    }
    libs
}

pub fn is_registered(path: &Path) -> bool {
    libraries().iter().any(|l| same_path(l, path))
}

fn new_content_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    let mixed = nanos ^ ((std::process::id() as u64) << 40) ^ 0x5DEE_CE66_D1CE_4E5B;
    // Steam's content ids are large positive 63-bit numbers
    (mixed & 0x7FFF_FFFF_FFFF_FFFF).max(1_000_000_000_000_000_000).to_string()
}

/// The marker file Steam keeps in a secondary library (`steamapps\libraryfolder.vdf`); returns its content id.
fn ensure_marker(library: &Path) -> Result<String, String> {
    let apps = library.join("steamapps");
    fs::create_dir_all(apps.join("common")).map_err(|e| format!("Could not create the Steam library folders: {e}"))?;
    let marker = apps.join("libraryfolder.vdf");
    if let Some(id) = fs::read_to_string(&marker).ok().and_then(|t| vdf::value(&t, "contentid")) {
        return Ok(id);
    }
    let id = new_content_id();
    fs::write(&marker, format!("\"libraryfolder\"\n{{\n\t\"contentid\"\t\t\"{id}\"\n\t\"label\"\t\t\"\"\n}}\n"))
        .map_err(|e| format!("Could not write the library marker: {e}"))?;
    Ok(id)
}

/// Add a library entry to the text of a libraryfolders.vdf file.
pub fn add_library_entry(text: &str, path: &Path, content_id: &str) -> Result<String, String> {
    // next free index: the top-level entries are "0", "1", ... each followed by a "{" line
    let lines: Vec<&str> = text.lines().collect();
    let mut next = 0u32;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.len() > 2 && t.starts_with('"') && t.ends_with('"') {
            if let Ok(n) = t[1..t.len() - 1].parse::<u32>() {
                if lines.get(i + 1).map(|l| l.trim() == "{").unwrap_or(false) {
                    next = next.max(n + 1);
                }
            }
        }
    }
    let close = text.rfind('}').ok_or("Steam's library list looks damaged (no closing brace).")?;
    let escaped = path.to_string_lossy().replace('\\', "\\\\");
    let entry = format!(
        "\t\"{next}\"\n\t{{\n\t\t\"path\"\t\t\"{escaped}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"contentid\"\t\t\"{content_id}\"\n\t\t\"totalsize\"\t\t\"0\"\n\t\t\"update_clean_bytes_tally\"\t\t\"0\"\n\t\t\"time_last_update_verified\"\t\t\"0\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n"
    );
    let mut out = String::with_capacity(text.len() + entry.len());
    out.push_str(text[..close].trim_end_matches(['\t', ' ']));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&entry);
    out.push_str(&text[close..]);
    // the new list must still read back correctly and contain every library it had before
    let before = vdf::values(text, "path").len();
    let after = vdf::values(&out, "path");
    if after.len() != before + 1 || !after.iter().any(|p| same_path(Path::new(p), path)) {
        return Err("Could not safely add the library to Steam's list.".into());
    }
    Ok(out)
}

/// Make Steam aware of a library folder (creating its structure if needed).
/// Steam must be closed - it rewrites this file while running. Returns true if it was added now.
pub fn register_library(library: &Path) -> Result<bool, String> {
    let root = steam_root().ok_or("Steam isn't installed on this PC yet.")?;
    if !is_registered(library) && crate::sys::steam_running() {
        return Err("Steam is running. Exit Steam completely (including the tray icon) so the new library can be added to it.".into());
    }
    register_library_in(&root, library)
}

/// The same, for a given Steam folder (separate so it can be tested on a fake Steam install).
fn register_library_in(root: &Path, library: &Path) -> Result<bool, String> {
    let content_id = ensure_marker(library)?;
    let listed = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf"))
        .map(|t| vdf::values(&t, "path").iter().any(|p| same_path(Path::new(p), library)))
        .unwrap_or(false);
    if listed || same_path(root, library) {
        return Ok(false);
    }
    let library = true_case(library.to_path_buf());
    let main_list = root.join("steamapps").join("libraryfolders.vdf");
    let lists = [main_list.clone(), root.join("config").join("libraryfolders.vdf")];
    for list in lists.iter().filter(|p| p.is_file() || **p == main_list) {
        let text = fs::read_to_string(list).unwrap_or_else(|_| {
            // no list yet (fresh Steam install): start one with the Steam folder itself as library 0
            let root_text = root.to_string_lossy().replace('\\', "\\\\");
            format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{root_text}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"contentid\"\t\t\"{}\"\n\t\t\"totalsize\"\t\t\"0\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n}}\n", new_content_id())
        });
        if vdf::values(&text, "path").iter().any(|p| same_path(Path::new(p), &library)) {
            continue;
        }
        let updated = add_library_entry(&text, &library, &content_id)?;
        if list.is_file() {
            let _ = fs::copy(list, list.with_extension("vdf.steamtransfer-backup"));
        }
        let tmp = list.with_extension("vdf.steamtransfer-new");
        fs::write(&tmp, &updated).map_err(|e| format!("Could not update Steam's library list: {e}"))?;
        fs::rename(&tmp, list).map_err(|e| format!("Could not update Steam's library list: {e}"))?;
    }
    Ok(true)
}

/// Create a Steam library where the user pointed (a drive, or a folder) and add it to Steam.
/// Returns the library folder and a note for the user.
pub fn create_library(selected: &Path) -> Result<(PathBuf, String), String> {
    let target = if looks_like_library(selected) {
        selected.to_path_buf()
    } else if selected.parent().is_none() || !selected.file_name().map(|n| n.to_string_lossy().to_lowercase().contains("steam")).unwrap_or(false) {
        // a drive root, or a folder not named for Steam: make the usual "SteamLibrary" inside it
        selected.join("SteamLibrary")
    } else {
        selected.to_path_buf()
    };
    ensure_marker(&target)?;
    let note = match register_library(&target) {
        Ok(true) => "Created and added to Steam. Steam will list it under Settings > Storage.".to_string(),
        Ok(false) => "Steam already knows this library.".to_string(),
        Err(e) if steam_root().is_none() => format!("Created. {e} Install Steam, and the library is added the next time you restore or move a game into it."),
        Err(e) => return Err(e),
    };
    Ok((true_case(target), note))
}

pub fn scan_all() -> Vec<Game> {
    let root = steam_root();
    let mut games: Vec<Game> = Vec::new();
    for lib in all_libraries().into_iter().map(|l| l.path) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // the file as Steam writes it: backslashes in paths are doubled
    const LIST: &str = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"contentid"		"3707994442925185161"
		"apps"
		{
			"228980"		"695366346"
		}
	}
}
"#;

    #[test]
    fn adds_a_library_to_steams_list() {
        let out = add_library_entry(LIST, Path::new(r"E:\SteamLibrary"), "1234567890123456789").unwrap();
        let paths = vdf::values(&out, "path");
        assert_eq!(paths, vec![r"C:\Program Files (x86)\Steam".to_string(), r"E:\SteamLibrary".to_string()]);
        assert!(out.contains("\t\"1\"\n\t{\n\t\t\"path\"\t\t\"E:\\\\SteamLibrary\""));
        assert_eq!(vdf::value(&out, "228980").as_deref(), Some("695366346"), "existing apps kept");
        assert_eq!(out.matches('{').count(), out.matches('}').count());
        // and a third one gets index 2
        let out2 = add_library_entry(&out, Path::new(r"F:\Games\SteamLibrary"), "42").unwrap();
        assert!(out2.contains("\t\"2\"\n"));
        assert_eq!(vdf::values(&out2, "path").len(), 3);
    }

    #[test]
    fn adds_to_a_copy_of_this_pcs_real_list() {
        // read-only: works on a copy of the real file if this PC has one
        let Some(root) = steam_root() else { return };
        let Ok(real) = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) else { return };
        let out = add_library_entry(&real, Path::new(r"Z:\SteamLibrary"), "1").unwrap();
        assert_eq!(vdf::values(&out, "path").len(), vdf::values(&real, "path").len() + 1);
        assert!(out.starts_with(&real[..real.rfind('}').unwrap()].trim_end_matches(['\t', ' '])));
    }

    #[test]
    fn backup_packages_are_not_libraries() {
        let root = std::env::temp_dir().join(format!("steam-transfer-libs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let library = root.join("SteamLibrary");
        fs::create_dir_all(library.join("steamapps").join("common")).unwrap();
        let package = root.join("Steam Transfer").join("Game (App 1)");
        fs::create_dir_all(package.join("steamapps").join("common")).unwrap();
        fs::write(package.join(crate::package::STATUS_FILE), "SUCCESS").unwrap();
        assert!(looks_like_library(&library));
        assert!(!looks_like_library(&package));
        assert!(!looks_like_library(&root));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore]
    fn print_libraries_on_this_pc() {
        for lib in all_libraries() {
            println!("{:<60} registered: {}", lib.path.display(), lib.registered);
        }
    }

    #[test]
    fn creates_and_registers_a_library_in_a_fake_steam() {
        let tmp = std::env::temp_dir().join(format!("steam-transfer-register-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let steam = tmp.join("Steam");
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::create_dir_all(steam.join("config")).unwrap();
        let list = LIST.replace(r"C:\\Program Files (x86)\\Steam", &steam.to_string_lossy().replace('\\', r"\\"));
        fs::write(steam.join("steamapps").join("libraryfolders.vdf"), &list).unwrap();
        fs::write(steam.join("config").join("libraryfolders.vdf"), &list).unwrap();
        let library = tmp.join("NewDrive").join("SteamLibrary");

        assert!(register_library_in(&steam, &library).unwrap(), "added the first time");
        assert!(library.join("steamapps").join("common").is_dir(), "folder structure created");
        let marker = fs::read_to_string(library.join("steamapps").join("libraryfolder.vdf")).unwrap();
        let id = vdf::value(&marker, "contentid").unwrap();
        for file in ["steamapps", "config"] {
            let text = fs::read_to_string(steam.join(file).join("libraryfolders.vdf")).unwrap();
            let paths = vdf::values(&text, "path");
            assert_eq!(paths.len(), 2, "{file}: both libraries listed");
            assert!(same_path(Path::new(&paths[1]), &library));
            assert!(text.contains(&id), "{file}: same content id as the marker");
            assert!(steam.join(file).join("libraryfolders.vdf.steamtransfer-backup").is_file(), "{file}: backup kept");
        }
        // a game manifest dropped in there is read back as installed in that library
        fs::write(
            library.join("steamapps").join("appmanifest_7.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"7\"\n\t\"name\"\t\t\"Seven\"\n\t\"installdir\"\t\t\"Seven\"\n}\n",
        )
        .unwrap();
        fs::create_dir_all(library.join("steamapps").join("common").join("Seven")).unwrap();
        assert_eq!(scan_library(&library).len(), 1);

        assert!(!register_library_in(&steam, &library).unwrap(), "not added twice");
        let text = fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")).unwrap();
        assert_eq!(vdf::values(&text, "path").len(), 2);
        let _ = fs::remove_dir_all(&tmp);
    }
}

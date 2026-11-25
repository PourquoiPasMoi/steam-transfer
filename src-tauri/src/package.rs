//! The on-disk backup format ("binder"). Compatible with Steam Transfer 1.x packages:
//!
//! <drive>\Steam Transfer\<Game Name> (App <id>)\
//!     steamapps\appmanifest_<id>.acf
//!     steamapps\common\<installdir>\...
//!     optional-user-data\...
//!     COPY-STATUS.txt  COPY-LOG.txt  COPY-ERROR.txt  README - Restore <Game>.txt
//!     cover.jpg               (new in 2.0 - artwork travels with the backup)
//!     steam-transfer.json     (new in 2.0 - machine-readable summary)

use crate::steam::{self, Game};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const BINDER_DIR: &str = "Steam Transfer";
pub const STATUS_FILE: &str = "COPY-STATUS.txt";
pub const META_FILE: &str = "steam-transfer.json";
pub const COVER_FILE: &str = "cover.jpg";

/// A destination the user picked -> the "Steam Transfer" folder inside it.
pub fn binder_root(selected: &Path) -> PathBuf {
    let is_binder = selected
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case(BINDER_DIR))
        .unwrap_or(false);
    if is_binder {
        selected.to_path_buf()
    } else {
        selected.join(BINDER_DIR)
    }
}

/// Same rule as 1.x: replace characters Windows does not allow in file names.
pub fn safe_name(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    cleaned.trim().trim_end_matches('.').to_string()
}

pub fn package_path(binder: &Path, game: &Game) -> PathBuf {
    binder.join(format!("{} (App {})", safe_name(&game.name), game.app_id))
}

/// Find an existing package for this app id even if the game was renamed since.
pub fn find_package(binder: &Path, game: &Game) -> PathBuf {
    let expected = package_path(binder, game);
    if expected.is_dir() {
        return expected;
    }
    let suffix = format!("(App {})", game.app_id);
    if let Ok(entries) = fs::read_dir(binder) {
        for entry in entries.flatten() {
            if entry.path().is_dir() && entry.file_name().to_string_lossy().ends_with(&suffix) {
                return entry.path();
            }
        }
    }
    expected
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Incomplete,
}

/// Last recognised status keyword in a status file (SUCCESS / IN PROGRESS / CANCELLED / FAILED).
pub fn read_status(path: &Path) -> Option<Status> {
    let text = fs::read_to_string(path).ok()?;
    parse_status(&text)
}

fn parse_status(text: &str) -> Option<Status> {
    // Steam Transfer 1.x wrote these files with a UTF-8 byte-order mark.
    let text = text.trim_start_matches('\u{feff}');
    let mut status = Status::Incomplete;
    for line in text.lines() {
        match line.trim().to_ascii_uppercase().as_str() {
            "SUCCESS" => status = Status::Complete,
            "IN PROGRESS" | "CANCELLED" | "FAILED" => status = Status::Incomplete,
            _ => {}
        }
    }
    Some(status)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    pub format: u32,
    pub app_id: String,
    pub name: String,
    pub install_dir: String,
    pub build_id: String,
    pub files: u64,
    pub bytes: u64,
    pub completed_at: String,
    pub app_version: String,
}

pub fn read_meta(package: &Path) -> Option<Meta> {
    serde_json::from_str(&fs::read_to_string(package.join(META_FILE)).ok()?).ok()
}

pub fn write_meta(package: &Path, meta: &Meta) -> std::io::Result<()> {
    fs::write(package.join(META_FILE), serde_json::to_string_pretty(meta).unwrap_or_default())
}

#[derive(Debug, Clone, Serialize)]
pub struct Package {
    pub path: PathBuf,
    pub app_id: String,
    pub name: String,
    pub install_dir: String,
    pub build_id: String,
    pub status: Status,
    pub bytes: u64,
    pub files: u64,
    pub completed_at: String,
    #[serde(skip)]
    pub game: Option<Game>,
    #[serde(skip)]
    pub art: Option<PathBuf>,
}

fn manifest_in(package: &Path) -> Option<PathBuf> {
    fs::read_dir(package.join("steamapps")).ok()?.flatten().map(|e| e.path()).find(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        name.starts_with("appmanifest_") && name.ends_with(".acf")
    })
}

fn app_id_from_folder(name: &str) -> Option<String> {
    let start = name.rfind("(App ")? + 5;
    let id = name[start..].trim_end_matches(')');
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then(|| id.to_string())
}

fn verified_bytes(status_file: &Path) -> Option<u64> {
    let text = fs::read_to_string(status_file).ok()?;
    text.trim_start_matches('\u{feff}').lines()
        .find_map(|l| l.trim().strip_prefix("Verified bytes:").map(|v| v.trim().to_string()))
        .and_then(|v| v.parse().ok())
}

pub fn read_package(path: &Path) -> Option<Package> {
    let folder = path.file_name()?.to_string_lossy().to_string();
    let status_file = path.join(STATUS_FILE);
    let meta = read_meta(path);
    let manifest = manifest_in(path);
    let game = manifest.as_deref().and_then(|m| steam::read_manifest(m, path));

    let app_id = game
        .as_ref()
        .map(|g| g.app_id.clone())
        .or_else(|| meta.as_ref().map(|m| m.app_id.clone()))
        .or_else(|| app_id_from_folder(&folder))?;
    let name = game
        .as_ref()
        .map(|g| g.name.clone())
        .or_else(|| meta.as_ref().map(|m| m.name.clone()))
        .unwrap_or_else(|| folder.rsplit_once(" (App ").map(|(n, _)| n.to_string()).unwrap_or(folder.clone()));

    let mut status = read_status(&status_file).unwrap_or(Status::Incomplete);
    if game.as_ref().map(|g| !g.game_path.is_dir()).unwrap_or(true) {
        status = Status::Incomplete;
    }
    let bytes = meta
        .as_ref()
        .filter(|m| m.bytes > 0)
        .map(|m| m.bytes)
        .or_else(|| verified_bytes(&status_file))
        .or_else(|| game.as_ref().map(|g| g.size_on_disk))
        .unwrap_or(0);

    let cover = path.join(COVER_FILE);
    let art = if cover.is_file() {
        Some(cover)
    } else {
        steam::steam_root().and_then(|r| steam::find_art(&r, &app_id))
    };

    Some(Package {
        path: path.to_path_buf(),
        install_dir: game.as_ref().map(|g| g.install_dir.clone()).unwrap_or_default(),
        build_id: game.as_ref().map(|g| g.build_id.clone()).unwrap_or_default(),
        files: meta.as_ref().map(|m| m.files).unwrap_or(0),
        completed_at: meta.as_ref().map(|m| m.completed_at.clone()).unwrap_or_default(),
        status,
        bytes,
        app_id,
        name,
        game,
        art,
    })
}

pub fn scan_binder(binder: &Path) -> Vec<Package> {
    let Ok(entries) = fs::read_dir(binder) else { return Vec::new() };
    let mut packages: Vec<Package> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| read_package(&e.path()))
        .collect();
    packages.sort_by_key(|p| p.name.to_lowercase());
    packages
}

pub fn readme_text(game: &Game, bytes: u64) -> String {
    let manifest = game.manifest_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let title = format!("{} STEAM TRANSFER BACKUP", game.name.to_uppercase());
    format!(
        "{title}\r\n{underline}\r\n\r\n\
Updated: {now}\r\nSteam app ID: {id}\r\nSteam build: {build}\r\nGame folder: {dir}\r\nCopied data: {size}\r\n\r\n\
EASIEST WAY TO RESTORE\r\n----------------------\r\n\
1. Install Steam on the other PC, sign in, and create a Steam Library (Settings > Storage).\r\n\
2. Exit Steam completely (including the tray icon).\r\n\
3. Run Steam Transfer, open the Restore tab, choose this drive and the target library.\r\n\
4. Start Steam once the game shows Restored. Allow it to verify or update if asked.\r\n\r\n\
MANUAL RESTORE\r\n--------------\r\n\
1. Exit Steam.\r\n\
2. Copy  steamapps\\common\\{dir}\r\n   to    <Steam Library>\\steamapps\\common\\{dir}\r\n\
3. Copy  steamapps\\{manifest}\r\n   to    <Steam Library>\\steamapps\\{manifest}\r\n\
4. Start Steam and use Properties > Installed Files > Verify integrity of game files.\r\n\r\n\
OPTIONAL USER DATA\r\n------------------\r\n\
optional-user-data holds save locations found automatically (Steam userdata and common AppData folders).\r\n\
Restores never overwrite saves automatically - copy them back by hand if you need them.\r\n\r\n\
This is a copy. The original installation was not removed.\r\n",
        underline = "=".repeat(title.len().min(70)),
        now = crate::now_string(),
        id = game.app_id,
        build = if game.build_id.is_empty() { "unknown" } else { &game.build_id },
        dir = game.install_dir,
        size = crate::format_bytes(bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_ids() {
        assert_eq!(safe_name("Call of Duty: Black Ops II"), "Call of Duty_ Black Ops II");
        assert_eq!(app_id_from_folder("Call of Duty_ Black Ops II (App 202970)").as_deref(), Some("202970"));
        assert_eq!(app_id_from_folder("Random Folder"), None);
        assert_eq!(binder_root(Path::new("D:\\")), PathBuf::from("D:\\Steam Transfer"));
        assert_eq!(binder_root(Path::new("D:\\Steam Transfer")), PathBuf::from("D:\\Steam Transfer"));
    }

    #[test]
    fn reads_v1_status_files_with_bom() {
        assert_eq!(parse_status("\u{feff}SUCCESS\r\nCompleted: 2026-09-18\r\n"), Some(Status::Complete));
        assert_eq!(parse_status("\u{feff}IN PROGRESS\r\n"), Some(Status::Incomplete));
        assert_eq!(parse_status("CANCELLED\r\nYou can safely resume\r\n"), Some(Status::Incomplete));
    }
}

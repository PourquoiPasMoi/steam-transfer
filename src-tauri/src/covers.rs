//! Cover art for downloads.
//!
//! Order of preference: an image the user picked, an image that came with the download
//! (cover / poster / folder picture, once it has finished), the closest match on the Steam store,
//! otherwise none (the UI draws a blank CD-R). Images are cached in the app data folder and
//! served through the app's art:// protocol, so they also work offline.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub type ArtMap = Arc<Mutex<HashMap<String, PathBuf>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    Custom { file: PathBuf },
    Included { file: PathBuf },
    Steam { app_id: String, title: String, file: PathBuf },
    /// looked up, nothing found
    Missing,
}

#[derive(Default, Serialize, Deserialize)]
struct Db {
    entries: HashMap<String, Entry>,
    /// downloads whose own files were already searched for a picture
    included_checked: HashSet<String>,
}

pub struct Covers {
    dir: PathBuf,
    db: Mutex<Db>,
    inflight: Mutex<HashSet<String>>,
    art: ArtMap,
}

/// Lowercase letters and digits only - the cache key for a download name.
pub fn normalize(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// Turn a download name like "Some.Game.v1.2.3-[Extras]_x64.iso" into a store search term ("Some Game").
pub fn search_term(name: &str) -> String {
    let mut s = name.trim().to_string();
    // drop a trailing file extension
    for ext in [".iso", ".zip", ".rar", ".7z", ".exe", ".mkv", ".mp4", ".avi", ".tar", ".gz", ".torrent"] {
        if s.to_lowercase().ends_with(ext) {
            s.truncate(s.len() - ext.len());
        }
    }
    // remove bracketed parts: [..] (..) {..}
    let mut out = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let spaced: String = out.chars().map(|c| if matches!(c, '_' | '+' | '|' | '~') { ' ' } else { c }).collect();
    let digits = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit());
    let v_number = |p: &str| p.len() > 1 && (p.starts_with('v') || p.starts_with('V')) && digits(&p[1..]);
    // Split dotted names ("Some.Game.v1.2") into words, and stop at the first part that is clearly
    // not title: a version (v1, 1.6.15), a long build number, or a platform/build tag.
    let mut kept: Vec<String> = Vec::new();
    'outer: for token in spaced.split_whitespace() {
        let parts: Vec<&str> = token.split('.').collect();
        for (i, raw) in parts.iter().enumerate() {
            let part = raw.trim_matches(|c: char| c == '-' || c == ',');
            let lower = part.to_lowercase();
            let next_is_digits = parts.get(i + 1).map(|n| digits(n.trim_matches('-'))).unwrap_or(false);
            let is_version = v_number(part) || (digits(part) && next_is_digits) || (part.len() >= 5 && digits(part));
            let is_tag = matches!(
                lower.as_str(),
                "build" | "update" | "x64" | "x86" | "win64" | "win32" | "windows" | "multi" | "proper" | "repack" | "dlc" | "dlcs"
            );
            if (is_version || is_tag) && !kept.is_empty() {
                break 'outer;
            }
            if !part.is_empty() {
                kept.push(part.to_string());
            }
        }
    }
    let term = kept.join(" ");
    let term = term.trim().trim_end_matches(['-', ':']).trim();
    term.chars().take(80).collect()
}

/// How alike two titles are, 0..1 (shared words, with a bonus when one contains the other).
pub fn similarity(a: &str, b: &str) -> f64 {
    let wa: HashSet<String> = words(a).into_iter().collect();
    let wb: HashSet<String> = words(b).into_iter().collect();
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let shared = wa.intersection(&wb).count() as f64;
    let jaccard = shared / wa.union(&wb).count() as f64;
    let (na, nb) = (normalize(a), normalize(b));
    let contains = !na.is_empty() && !nb.is_empty() && (na.contains(&nb) || nb.contains(&na));
    if contains {
        jaccard.max(0.75)
    } else {
        jaccard
    }
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .map(|e| matches!(e.to_string_lossy().to_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp"))
        .unwrap_or(false)
}

/// Best picture inside a finished download's folder (cover/poster/folder/front first, then the largest).
fn find_included_image(folder: &Path) -> Option<PathBuf> {
    let mut best: Option<(i32, u64, PathBuf)> = None;
    let mut stack = vec![(folder.to_path_buf(), 0)];
    let mut seen = 0;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 3000 {
                break;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if depth < 3 {
                    stack.push((path, depth + 1));
                }
            } else if is_image(&path) && (20_000..30_000_000).contains(&meta.len()) {
                let name = path.file_stem().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                let score = ["cover", "poster", "folder", "front", "box", "art"].iter().filter(|k| name.contains(*k)).count() as i32 * 10
                    - depth;
                let better = match &best {
                    None => true,
                    Some((s, len, _)) => score > *s || (score == *s && meta.len() > *len),
                };
                if better {
                    best = Some((score, meta.len(), path));
                }
            }
        }
    }
    best.map(|(_, _, p)| p)
}

impl Covers {
    pub fn new(data_dir: &Path, art: ArtMap) -> Arc<Self> {
        let dir = data_dir.join("covers");
        let db = std::fs::read_to_string(dir.join("covers.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Arc::new(Covers { dir, db: Mutex::new(db), inflight: Mutex::new(HashSet::new()), art })
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(text) = serde_json::to_string_pretty(&*self.db.lock().unwrap()) {
            let _ = std::fs::write(self.dir.join("covers.json"), text);
        }
    }

    fn url(&self, key: &str, file: &Path) -> Option<String> {
        if !file.is_file() {
            return None;
        }
        let art_key = format!("cover-{key}");
        self.art.lock().unwrap().insert(art_key.clone(), file.to_path_buf());
        // the file name changes when the image does, so the webview doesn't show a stale cached copy
        let stamp = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        Some(format!("http://art.localhost/{art_key}?v={stamp}"))
    }

    /// The cover URL for a download, if one is known. Starts a background lookup the first time a
    /// name is seen, and checks the download's own files for a picture once it has finished.
    /// `own_folder` must only be given when the folder belongs to this download alone.
    pub fn url_for(self: &Arc<Self>, name: &str, own_folder: Option<&Path>, finished: bool) -> Option<String> {
        let key = normalize(name);
        if key.len() < 2 {
            return None;
        }
        if finished {
            if let Some(folder) = own_folder {
                let check = {
                    let db = self.db.lock().unwrap();
                    !db.included_checked.contains(&key) && !matches!(db.entries.get(&key), Some(Entry::Custom { .. }))
                };
                if check {
                    let found = find_included_image(folder);
                    let mut db = self.db.lock().unwrap();
                    db.included_checked.insert(key.clone());
                    if let Some(src) = found {
                        let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "jpg".into());
                        let dest = self.dir.join(format!("included-{key}.{ext}"));
                        let _ = std::fs::create_dir_all(&self.dir);
                        if std::fs::copy(&src, &dest).is_ok() {
                            db.entries.insert(key.clone(), Entry::Included { file: dest });
                        }
                    }
                    drop(db);
                    self.save();
                }
            }
        }
        let entry = self.db.lock().unwrap().entries.get(&key).cloned();
        match entry {
            Some(Entry::Custom { file }) | Some(Entry::Included { file }) | Some(Entry::Steam { file, .. }) => self.url(&key, &file),
            Some(Entry::Missing) => None,
            None => {
                self.start_lookup(name.to_string(), key);
                None
            }
        }
    }

    fn start_lookup(self: &Arc<Self>, name: String, key: String) {
        if !self.inflight.lock().unwrap().insert(key.clone()) {
            return;
        }
        let this = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let result = this.lookup_steam(&name, &key).await;
            {
                let mut db = this.db.lock().unwrap();
                // a picture the user chose meanwhile wins
                if !db.entries.contains_key(&key) {
                    db.entries.insert(key.clone(), result.unwrap_or(Entry::Missing));
                }
            }
            this.save();
            this.inflight.lock().unwrap().remove(&key);
        });
    }

    async fn lookup_steam(&self, name: &str, key: &str) -> Option<Entry> {
        let term = search_term(name);
        if term.len() < 2 {
            return None;
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(12))
            .user_agent(concat!("SteamTransfer/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        let url = tauri::Url::parse_with_params(
            "https://store.steampowered.com/api/storesearch/",
            &[("term", term.as_str()), ("l", "english"), ("cc", "US")],
        )
        .ok()?;
        let body: serde_json::Value = client.get(url).send().await.ok()?.json().await.ok()?;
        let (app_id, title, score) = body["items"]
            .as_array()?
            .iter()
            .filter(|i| i["type"] == "app")
            .filter_map(|i| {
                let title = i["name"].as_str()?.to_string();
                let id = i["id"].as_u64()?.to_string();
                Some((id, title.clone(), similarity(&term, &title)))
            })
            .max_by(|a, b| a.2.total_cmp(&b.2))?;
        if score < 0.5 {
            return None;
        }
        let _ = std::fs::create_dir_all(&self.dir);
        let candidates = [
            format!("https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/{app_id}/library_600x900.jpg"),
            format!("https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/{app_id}/header.jpg"),
        ];
        for (i, url) in candidates.iter().enumerate() {
            let Ok(resp) = client.get(url).send().await else { continue };
            let is_image = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.starts_with("image/"))
                .unwrap_or(false);
            if !resp.status().is_success() || !is_image {
                continue;
            }
            let Ok(bytes) = resp.bytes().await else { continue };
            if bytes.len() < 2000 {
                continue;
            }
            let file = self.dir.join(format!("steam-{app_id}-{}.jpg", if i == 0 { "cover" } else { "header" }));
            if std::fs::write(&file, &bytes).is_ok() {
                let _ = key;
                return Some(Entry::Steam { app_id, title, file });
            }
        }
        None
    }

    /// Use a picture the user chose.
    pub fn set_custom(&self, name: &str, source: &Path) -> Result<(), String> {
        let key = normalize(name);
        if !is_image(source) {
            return Err("Pick a JPG, PNG or WebP image.".into());
        }
        let ext = source.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_else(|| "jpg".into());
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        // unique file name so the new picture shows straight away
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let dest = self.dir.join(format!("custom-{key}-{stamp}.{ext}"));
        std::fs::copy(source, &dest).map_err(|e| format!("Could not copy the picture: {e}"))?;
        let old = self.db.lock().unwrap().entries.insert(key, Entry::Custom { file: dest });
        if let Some(Entry::Custom { file }) = old {
            let _ = std::fs::remove_file(file);
        }
        self.save();
        Ok(())
    }

    /// Forget the cover (and the user's picture) so it is looked up again.
    pub fn reset(&self, name: &str) {
        let key = normalize(name);
        let mut db = self.db.lock().unwrap();
        if let Some(Entry::Custom { file }) = db.entries.remove(&key) {
            let _ = std::fs::remove_file(file);
        }
        db.included_checked.remove(&key);
        drop(db);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_download_names_into_search_terms() {
        assert_eq!(search_term("Some.Great.Game.v1.2.3-[Extras]_x64.iso"), "Some Great Game");
        assert_eq!(search_term("Hollow Knight (2017) [GOG]"), "Hollow Knight");
        assert_eq!(search_term("Celeste Build 12345678"), "Celeste");
        assert_eq!(search_term("Stardew Valley v1.6.15"), "Stardew Valley");
        assert_eq!(search_term("Stardew_Valley_1.6.15_(64bit)"), "Stardew Valley");
        assert_eq!(search_term("Call of Duty: Black Ops II - Zombies"), "Call of Duty: Black Ops II Zombies");
    }

    #[test]
    fn scores_title_matches() {
        assert!(similarity("Grand Theft Auto V", "Grand Theft Auto V Enhanced") >= 0.75);
        assert!(similarity("Hollow Knight", "Hollow Knight") > 0.99);
        assert!(similarity("debian netinst", "Grand Theft Auto V") < 0.2);
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Real Steam lookup (not run by default): cargo test --lib real_cover_lookup -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_cover_lookup() {
        let root = std::env::temp_dir().join(format!("steam-transfer-covers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let covers = Covers::new(&root, ArtMap::default());
        for name in ["Hollow.Knight.v1.5.78-[GOG]", "Stardew_Valley_1.6.15_(64bit)", "debian-13.8.0-amd64-netinst.iso"] {
            let mut url = None;
            for _ in 0..40 {
                url = covers.url_for(name, None, false);
                if url.is_some() || matches!(covers.db.lock().unwrap().entries.get(&normalize(name)), Some(Entry::Missing)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let entry = covers.db.lock().unwrap().entries.get(&normalize(name)).cloned();
            println!("{name:<40} -> term {:?}  {:?}  url {:?}", search_term(name), entry, url);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

//! Torrent downloads (magnet links and .torrent files), powered by librqbit.
//!
//! Adding works in two steps so files land where the user expects (like qBittorrent):
//! first the torrent's metadata is fetched ("list only"), which tells us whether it is a
//! single file or a folder; then it is added for real with the chosen folder (plus the
//! torrent's own sub-folder when it has several files). While metadata is being fetched the
//! entry shows as "pending" in the list.
//!
//! The session is persisted in the app data folder, so unfinished downloads resume when the
//! app starts again.

use crate::covers::{self, Covers};
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions, SessionPersistenceConfig};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

#[derive(Clone)]
pub enum Source {
    Link(String),
    File(Vec<u8>),
}

impl Source {
    fn to_add(&self) -> AddTorrent<'static> {
        match self {
            Source::Link(url) => AddTorrent::from_url(url.clone()),
            Source::File(bytes) => AddTorrent::from_bytes(bytes.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TorrentDto {
    /// "t<id>" for torrents in the session, "p<n>" for ones still fetching metadata
    pub key: String,
    pub id: Option<usize>,
    pub name: String,
    /// fetching | checking | downloading | seeding | paused | error
    pub state: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    pub down_bps: u64,
    pub up_bps: u64,
    pub eta_secs: Option<u64>,
    pub peers: u32,
    pub seeders: u32,
    pub error: Option<String>,
    pub folder: String,
    /// cover image URL (picked by the user, included in the download, or matched on Steam)
    pub cover: Option<String>,
}

/// A download still fetching its metadata (not yet known to librqbit). Saved to pending.json
/// so it survives restarts, and can be paused (its fetch task is stopped) and resumed.
struct Pending {
    key: String,
    name: String,
    folder: PathBuf,
    source: Source,
    paused: bool,
    error: Option<String>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

#[derive(Serialize, serde::Deserialize)]
struct SavedPending {
    key: String,
    name: String,
    folder: PathBuf,
    link: Option<String>,
    file: Option<PathBuf>,
    paused: bool,
}

/// Speed limits in bytes per second (None = unlimited).
#[derive(Debug, Clone, Copy, Default, Serialize, serde::Deserialize)]
pub struct Limits {
    pub down_bps: Option<u32>,
    pub up_bps: Option<u32>,
}

pub struct Torrents {
    session: OnceCell<Arc<Session>>,
    data_dir: PathBuf,
    default_dir: PathBuf,
    pending: Mutex<Vec<Pending>>,
    next: AtomicU64,
    covers: Arc<Covers>,
    limits: Mutex<Limits>,
}

/// Display name from a magnet link's `dn=` parameter, if any.
fn magnet_name(link: &str) -> Option<String> {
    let query = link.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "dn").then(|| covers::decode_entities(&percent_decode(&v.replace('+', " "))))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The torrent's display name from a .torrent file's bytes, if it can be read.
fn torrent_file_name(bytes: &[u8]) -> Option<String> {
    let meta = librqbit::torrent_from_bytes(bytes).ok()?;
    meta.info.data.name.as_ref().map(|n| String::from_utf8_lossy(n.as_ref()).into_owned())
}

impl Torrents {
    pub fn new(data_dir: PathBuf, default_dir: PathBuf, covers: Arc<Covers>) -> Arc<Self> {
        Arc::new(Torrents {
            covers,
            session: OnceCell::new(),
            data_dir,
            default_dir,
            pending: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
            limits: Mutex::new(Limits::default()),
        })
    }

    fn apply_limits(session: &Session, limits: Limits) {
        session.ratelimits.set_download_bps(limits.down_bps.and_then(std::num::NonZeroU32::new));
        session.ratelimits.set_upload_bps(limits.up_bps.and_then(std::num::NonZeroU32::new));
    }

    /// Set download/upload limits; applies to every download straight away.
    pub fn set_limits(&self, limits: Limits) {
        let old = std::mem::replace(&mut *self.limits.lock().unwrap(), limits);
        let Some(session) = self.session.get() else { return };
        Self::apply_limits(session, limits);
        // Peers already have many requests queued, which would keep arriving at full speed for a
        // while. When a limit gets tighter, briefly pause and resume active downloads to drop that
        // backlog so the new limit takes effect within seconds.
        let tighter = |old: Option<u32>, new: Option<u32>| match (old, new) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(o), Some(n)) => n < o,
        };
        if tighter(old.down_bps, limits.down_bps) || tighter(old.up_bps, limits.up_bps) {
            let session = Arc::clone(session);
            tauri::async_runtime::spawn(async move {
                let active: Vec<Arc<librqbit::ManagedTorrent>> = session.with_torrents(|iter| {
                    iter.filter(|(_, h)| matches!(h.stats().state, librqbit::TorrentStatsState::Live))
                        .map(|(_, h)| Arc::clone(h))
                        .collect()
                });
                for handle in active {
                    if session.pause(&handle).await.is_ok() {
                        let _ = session.unpause(&handle).await;
                    }
                }
            });
        }
    }

    pub fn limits(&self) -> Limits {
        *self.limits.lock().unwrap()
    }

    fn pending_file(&self) -> PathBuf {
        self.data_dir.join("pending.json")
    }

    fn save_pending(&self) {
        let pending = self.pending.lock().unwrap();
        let dir = self.data_dir.join("pending");
        let saved: Vec<SavedPending> = pending
            .iter()
            .map(|p| {
                let (link, file) = match &p.source {
                    Source::Link(l) => (Some(l.clone()), None),
                    Source::File(bytes) => {
                        let path = dir.join(format!("{}.torrent", p.key));
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::write(&path, bytes);
                        (None, Some(path))
                    }
                };
                SavedPending { key: p.key.clone(), name: p.name.clone(), folder: p.folder.clone(), link, file, paused: p.paused }
            })
            .collect();
        let _ = std::fs::create_dir_all(&self.data_dir);
        let _ = std::fs::write(self.pending_file(), serde_json::to_string_pretty(&saved).unwrap_or_default());
    }

    /// Bring back downloads that were still fetching their info when the app closed.
    pub fn restore_pending(self: &Arc<Self>) {
        let Some(saved) = std::fs::read_to_string(self.pending_file())
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<SavedPending>>(&t).ok())
        else {
            return;
        };
        let mut highest = 0;
        let mut to_start = Vec::new();
        {
            let mut pending = self.pending.lock().unwrap();
            for s in saved {
                let source = match (s.link, s.file.and_then(|f| std::fs::read(f).ok())) {
                    (Some(link), _) => Source::Link(link),
                    (None, Some(bytes)) => Source::File(bytes),
                    _ => continue,
                };
                highest = highest.max(s.key.trim_start_matches('p').parse::<u64>().unwrap_or(0));
                if !s.paused {
                    to_start.push(s.key.clone());
                }
                pending.push(Pending { key: s.key, name: s.name, folder: s.folder, source, paused: s.paused, error: None, task: None });
            }
        }
        self.next.fetch_max(highest + 1, Ordering::Relaxed);
        for key in to_start {
            self.start_pending(&key);
        }
    }

    /// (Re)start fetching a pending download's info in the background.
    fn start_pending(self: &Arc<Self>, key: &str) {
        let (source, folder) = {
            let mut pending = self.pending.lock().unwrap();
            let Some(p) = pending.iter_mut().find(|p| p.key == key) else { return };
            p.paused = false;
            p.error = None;
            (p.source.clone(), p.folder.clone())
        };
        let this = Arc::clone(self);
        let task_key = key.to_string();
        let task = tauri::async_runtime::spawn(async move {
            let result = this.add_inner(source, folder).await;
            {
                let mut pending = this.pending.lock().unwrap();
                match result {
                    Ok(()) => pending.retain(|p| p.key != task_key),
                    Err(e) => {
                        if let Some(p) = pending.iter_mut().find(|p| p.key == task_key) {
                            p.error = Some(e);
                            p.task = None;
                        }
                    }
                }
            }
            this.save_pending();
        });
        if let Some(p) = self.pending.lock().unwrap().iter_mut().find(|p| p.key == key) {
            p.task = Some(task);
        }
        self.save_pending();
    }

    fn persistence_dir(&self) -> PathBuf {
        self.data_dir.join("torrents")
    }

    /// True if earlier downloads were saved, so the session should start with the app.
    pub fn has_saved_session(&self) -> bool {
        self.persistence_dir().is_dir()
    }

    pub fn default_dir(&self) -> &Path {
        &self.default_dir
    }

    pub async fn session(&self) -> Result<Arc<Session>, String> {
        self.session
            .get_or_try_init(|| async {
                let _ = std::fs::create_dir_all(&self.default_dir);
                // Peer discovery (DHT) state lives in our own data folder, not the library's shared default.
                let options = |dht_file: Option<PathBuf>| SessionOptions {
                    persistence: Some(SessionPersistenceConfig::Json { folder: Some(self.persistence_dir()) }),
                    fastresume: true,
                    dht: Some(librqbit::DhtSessionConfig {
                        persistence: dht_file.map(|f| librqbit::dht::DhtPersistenceConfig { config_filename: Some(f), dump_interval: None }),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let session = match Session::new_with_opts(self.default_dir.clone(), options(Some(self.data_dir.join("dht.json")))).await {
                    Ok(session) => session,
                    // e.g. the saved DHT port is taken by another program: start on a random free port instead
                    Err(_) => Session::new_with_opts(self.default_dir.clone(), options(None))
                        .await
                        .map_err(|e| format!("Could not start the torrent engine: {e:#}"))?,
                };
                Self::apply_limits(&session, self.limits());
                Ok::<_, String>(session)
            })
            .await
            .cloned()
    }

    /// Start adding a torrent. Returns immediately; the entry shows as pending until its
    /// metadata has been fetched, then becomes a normal download.
    pub fn add(self: &Arc<Self>, source: Source, folder: PathBuf) -> Result<String, String> {
        if let Source::Link(link) = &source {
            let l = link.trim().to_ascii_lowercase();
            if !(l.starts_with("magnet:?") || l.starts_with("http://") || l.starts_with("https://")) {
                return Err("That doesn't look like a magnet link (it should start with magnet:?).".into());
            }
        }
        std::fs::create_dir_all(&folder).map_err(|e| format!("Can't use that folder: {e}"))?;
        let key = format!("p{}", self.next.fetch_add(1, Ordering::Relaxed));
        let name = match &source {
            Source::Link(link) => magnet_name(link).unwrap_or_else(|| "Fetching torrent info…".into()),
            Source::File(bytes) => torrent_file_name(bytes).unwrap_or_else(|| "Reading torrent…".into()),
        };
        self.pending.lock().unwrap().push(Pending {
            key: key.clone(),
            name,
            folder,
            source,
            paused: false,
            error: None,
            task: None,
        });
        self.start_pending(&key);
        Ok(key)
    }

    async fn add_inner(&self, source: Source, folder: PathBuf) -> Result<(), String> {
        let session = self.session().await?;
        let listed = session
            .add_torrent(source.to_add(), Some(AddTorrentOptions { list_only: true, ..Default::default() }))
            .await
            .map_err(|e| format!("{e:#}"))?;
        let info = match listed {
            AddTorrentResponse::ListOnly(info) => info,
            AddTorrentResponse::AlreadyManaged(..) | AddTorrentResponse::Added(..) => {
                return Err("This torrent is already in your downloads.".into())
            }
        };
        // Multi-file torrents get their own sub-folder (librqbit computes it relative to its default folder).
        let sub = info.output_folder.strip_prefix(&self.default_dir).map(Path::to_path_buf).unwrap_or_default();
        let output = folder.join(sub);
        session
            .add_torrent(
                AddTorrent::from_bytes(info.torrent_bytes),
                Some(AddTorrentOptions {
                    output_folder: Some(output.to_string_lossy().into_owned()),
                    // lets an interrupted or re-added download continue from the files already on disk
                    overwrite: true,
                    initial_peers: Some(info.seen_peers),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| format!("{e:#}"))?;
        Ok(())
    }

    pub async fn list(&self) -> Vec<TorrentDto> {
        let mut out: Vec<TorrentDto> = self
            .pending
            .lock()
            .unwrap()
            .iter()
            .map(|p| TorrentDto {
                key: p.key.clone(),
                id: None,
                name: p.name.clone(),
                state: if p.error.is_some() { "error" } else if p.paused { "paused" } else { "fetching" }.into(),
                progress_bytes: 0,
                total_bytes: 0,
                uploaded_bytes: 0,
                down_bps: 0,
                up_bps: 0,
                eta_secs: None,
                peers: 0,
                seeders: 0,
                error: p.error.clone(),
                folder: p.folder.to_string_lossy().into_owned(),
                cover: if p.name.ends_with('…') { None } else { self.covers.url_for(&p.name, None, false) },
            })
            .collect();

        let Some(session) = self.session.get() else { return out };
        let mut items: Vec<TorrentDto> = session.with_torrents(|iter| {
            iter.map(|(id, handle)| {
                let stats = handle.stats();
                let live = serde_json::to_value(&stats.live).unwrap_or_default();
                let peer = |field: &str| live["snapshot"]["peer_stats"][field].as_u64().unwrap_or(0) as u32;
                let (down, up) = stats
                    .live
                    .as_ref()
                    .map(|l| (l.download_speed.as_bytes(), l.upload_speed.as_bytes()))
                    .unwrap_or((0, 0));
                let state = if stats.error.is_some() {
                    "error"
                } else {
                    match &stats.state {
                        librqbit::TorrentStatsState::Initializing { .. } => "checking",
                        librqbit::TorrentStatsState::Paused => "paused",
                        librqbit::TorrentStatsState::Error => "error",
                        librqbit::TorrentStatsState::Live if stats.finished => "seeding",
                        librqbit::TorrentStatsState::Live => "downloading",
                    }
                };
                let remaining = stats.total_bytes.saturating_sub(stats.progress_bytes);
                let name = handle.name().unwrap_or_else(|| "Torrent".into());
                let folder = handle.output_folder();
                // a multi-file torrent has a folder of its own (named after it); a single file shares the save folder
                let own_folder = folder
                    .file_name()
                    .map(|f| covers::normalize(&f.to_string_lossy()) == covers::normalize(&name))
                    .unwrap_or(false)
                    .then_some(folder);
                let cover = self.covers.url_for(&name, own_folder, stats.finished);
                TorrentDto {
                    key: format!("t{id}"),
                    id: Some(id),
                    cover,
                    name,
                    state: state.into(),
                    progress_bytes: stats.progress_bytes,
                    total_bytes: stats.total_bytes,
                    uploaded_bytes: stats.uploaded_bytes,
                    down_bps: down,
                    up_bps: up,
                    eta_secs: (state == "downloading" && down > 0).then(|| remaining / down.max(1)),
                    peers: peer("live"),
                    seeders: peer("live_seeders"),
                    error: stats.error.clone(),
                    folder: folder.to_string_lossy().into_owned(),
                }
            })
            .collect()
        });
        items.sort_by_key(|t| t.id);
        out.extend(items);
        out
    }

    async fn handle(&self, id: usize) -> Result<(Arc<Session>, Arc<librqbit::ManagedTorrent>), String> {
        let session = self.session().await?;
        let handle = session.get(TorrentIdOrHash::Id(id)).ok_or("That download no longer exists.")?;
        Ok((session, handle))
    }

    fn session_id(key: &str) -> Result<usize, String> {
        key.trim_start_matches('t').parse().map_err(|_| "Unknown download.".to_string())
    }

    /// Pause at any stage: while fetching info (stops the fetch), checking files or downloading.
    pub async fn pause(self: &Arc<Self>, key: &str) -> Result<(), String> {
        if key.starts_with('p') {
            {
                let mut pending = self.pending.lock().unwrap();
                let p = pending.iter_mut().find(|p| p.key == key).ok_or("That download no longer exists.")?;
                if let Some(task) = p.task.take() {
                    task.abort();
                }
                p.paused = true;
            }
            self.save_pending();
            return Ok(());
        }
        let (session, handle) = self.handle(Self::session_id(key)?).await?;
        session.pause(&handle).await.map_err(|e| format!("{e:#}"))
    }

    pub async fn resume(self: &Arc<Self>, key: &str) -> Result<(), String> {
        if key.starts_with('p') {
            self.start_pending(key);
            return Ok(());
        }
        let (session, handle) = self.handle(Self::session_id(key)?).await?;
        session.unpause(&handle).await.map_err(|e| format!("{e:#}"))
    }

    pub async fn remove(&self, key: &str, delete_files: bool) -> Result<(), String> {
        if key.starts_with('p') {
            {
                let mut pending = self.pending.lock().unwrap();
                if let Some(p) = pending.iter_mut().find(|p| p.key == key) {
                    if let Some(task) = p.task.take() {
                        task.abort();
                    }
                }
                pending.retain(|p| p.key != key);
            }
            let _ = std::fs::remove_file(self.data_dir.join("pending").join(format!("{key}.torrent")));
            self.save_pending();
            return Ok(());
        }
        let id: usize = key.trim_start_matches('t').parse().map_err(|_| "Unknown download.")?;
        let session = self.session().await?;
        session.delete(TorrentIdOrHash::Id(id), delete_files).await.map_err(|e| format!("{e:#}"))
    }

    /// True while something is still downloading (used to warn before closing the app).
    pub async fn downloading(&self) -> bool {
        self.list().await.iter().any(|t| matches!(t.state.as_str(), "downloading" | "checking" | "fetching"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_magnet_display_name() {
        assert_eq!(
            magnet_name("magnet:?xt=urn:btih:abc&dn=Big+Buck+Bunny%20(1080p)&tr=udp%3A%2F%2Ftracker").as_deref(),
            Some("Big Buck Bunny (1080p)")
        );
        assert_eq!(magnet_name("magnet:?xt=urn:btih:abc"), None);
        assert_eq!(percent_decode("100%"), "100%");
    }

    /// Real network test (not run by default):
    /// TORRENT_TEST_URL=<magnet or .torrent URL> cargo test --lib real_download -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_download() {
        let url = std::env::var("TORRENT_TEST_URL").expect("set TORRENT_TEST_URL");
        let root = std::env::temp_dir().join(format!("steam-transfer-torrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let art: covers::ArtMap = Default::default();
        let torrents = Torrents::new(root.join("data"), root.join("default"), Covers::new(&root.join("data"), art));
        let out = root.join("downloads");
        tauri::async_runtime::block_on(async {
            torrents.add(Source::Link(url), out.clone()).unwrap();
            let mut best = 0u64;
            for second in 0..90 {
                tokio_sleep(1000).await;
                let list = torrents.list().await;
                let t = &list[0];
                best = best.max(t.progress_bytes);
                println!(
                    "{second:>3}s {:<12} {:>5.1}% {:>10}/s peers {:>3} ({} seeders) {}",
                    t.state,
                    if t.total_bytes > 0 { t.progress_bytes as f64 * 100.0 / t.total_bytes as f64 } else { 0.0 },
                    crate::format_bytes(t.down_bps),
                    t.peers,
                    t.seeders,
                    t.error.clone().unwrap_or_default()
                );
                if best > 50 * 1024 * 1024 {
                    break;
                }
            }
            let list = torrents.list().await;
            println!("saved to {}", list[0].folder);
            assert!(best > 0, "nothing downloaded");
            torrents.remove(&list[0].key, true).await.unwrap();
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real network test: pause while fetching info, resume, then a 1 MB/s speed limit, then pause/resume.
    /// TORRENT_TEST_URL=<magnet or .torrent URL> cargo test --lib real_pause_and_limit -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_pause_and_limit() {
        let url = std::env::var("TORRENT_TEST_URL").expect("set TORRENT_TEST_URL");
        let root = std::env::temp_dir().join(format!("steam-transfer-torrent2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let art: covers::ArtMap = Default::default();
        let torrents = Torrents::new(root.join("data"), root.join("default"), Covers::new(&root.join("data"), art));
        tauri::async_runtime::block_on(async {
            let key = torrents.add(Source::Link(url), root.join("downloads")).unwrap();
            // pause straight away, while it is still fetching info
            torrents.pause(&key).await.unwrap();
            tokio_sleep(3000).await;
            let list = torrents.list().await;
            println!("after pause while fetching: {} entries, state {} error {:?}", list.len(), list[0].state, list[0].error);
            assert_eq!(list[0].state, "paused");
            assert!(std::fs::read_to_string(root.join("data").join("pending.json")).unwrap().contains("\"paused\": true"));
            let preset = std::env::var("TORRENT_TEST_PRESET_LIMIT").is_ok();
            if preset {
                torrents.set_limits(Limits { down_bps: Some(1024 * 1024), up_bps: Some(64 * 1024) });
                println!("   -> limit set to 1 MB/s BEFORE starting");
            }
            torrents.resume(&key).await.unwrap();
            let mut speeds = if preset { vec![0] } else { Vec::new() };
            let mut last = 0u64;
            for second in 0..40 {
                tokio_sleep(1000).await;
                let list = torrents.list().await;
                let t = &list[0];
                // measure real throughput from the bytes downloaded, not the engine's smoothed speed figure
                let delta = t.progress_bytes.saturating_sub(last);
                last = t.progress_bytes;
                println!("{second:>3}s {:<12} reported {:>10}/s   actual {:>10}/s", t.state, crate::format_bytes(t.down_bps), crate::format_bytes(delta));
                if t.state == "downloading" && second >= 8 && speeds.is_empty() {
                    torrents.set_limits(Limits { down_bps: Some(1024 * 1024), up_bps: Some(64 * 1024) });
                    println!("   -> limit set to 1 MB/s down, 64 KB/s up");
                    speeds.push(0);
                } else if !speeds.is_empty() && t.state == "downloading" {
                    speeds.push(delta);
                }
                if speeds.len() > 12 {
                    break;
                }
            }
            let settled: Vec<u64> = speeds.iter().rev().take(5).cloned().collect();
            let avg = settled.iter().sum::<u64>() / settled.len().max(1) as u64;
            println!("average speed with the limit: {}/s", crate::format_bytes(avg));
            assert!(avg < 1_400_000, "limit not applied");
            let list = torrents.list().await;
            let real_key = list[0].key.clone();
            torrents.pause(&real_key).await.unwrap();
            tokio_sleep(2000).await;
            let state = torrents.list().await[0].state.clone();
            println!("after pausing the download: {state}");
            assert_eq!(state, "paused");
            torrents.resume(&real_key).await.unwrap();
            tokio_sleep(2000).await;
            println!("after resuming: {}", torrents.list().await[0].state);
            torrents.remove(&real_key, true).await.unwrap();
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    async fn tokio_sleep(ms: u64) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let _ = tx.send(());
        });
        let _ = rx.await;
    }
}

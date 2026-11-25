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

struct Pending {
    key: String,
    name: String,
    folder: PathBuf,
    error: Option<String>,
}

pub struct Torrents {
    session: OnceCell<Arc<Session>>,
    data_dir: PathBuf,
    default_dir: PathBuf,
    pending: Mutex<Vec<Pending>>,
    next: AtomicU64,
    covers: Arc<Covers>,
}

/// Display name from a magnet link's `dn=` parameter, if any.
fn magnet_name(link: &str) -> Option<String> {
    let query = link.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "dn").then(|| percent_decode(&v.replace('+', " ")))
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

impl Torrents {
    pub fn new(data_dir: PathBuf, default_dir: PathBuf, covers: Arc<Covers>) -> Arc<Self> {
        Arc::new(Torrents {
            covers,
            session: OnceCell::new(),
            data_dir,
            default_dir,
            pending: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        })
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
                let opts = SessionOptions {
                    persistence: Some(SessionPersistenceConfig::Json { folder: Some(self.persistence_dir()) }),
                    fastresume: true,
                    ..Default::default()
                };
                Session::new_with_opts(self.default_dir.clone(), opts)
                    .await
                    .map_err(|e| format!("Could not start the torrent engine: {e:#}"))
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
            Source::File(_) => "Reading torrent…".into(),
        };
        self.pending.lock().unwrap().push(Pending { key: key.clone(), name, folder: folder.clone(), error: None });

        let this = Arc::clone(self);
        let task_key = key.clone();
        tauri::async_runtime::spawn(async move {
            let result = this.add_inner(source, folder).await;
            let mut pending = this.pending.lock().unwrap();
            match result {
                Ok(()) => pending.retain(|p| p.key != task_key),
                Err(e) => {
                    if let Some(p) = pending.iter_mut().find(|p| p.key == task_key) {
                        p.error = Some(e);
                    }
                }
            }
        });
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
                state: if p.error.is_some() { "error" } else { "fetching" }.into(),
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

    pub async fn pause(&self, id: usize) -> Result<(), String> {
        let (session, handle) = self.handle(id).await?;
        session.pause(&handle).await.map_err(|e| format!("{e:#}"))
    }

    pub async fn resume(&self, id: usize) -> Result<(), String> {
        let (session, handle) = self.handle(id).await?;
        session.unpause(&handle).await.map_err(|e| format!("{e:#}"))
    }

    pub async fn remove(&self, key: &str, delete_files: bool) -> Result<(), String> {
        if key.starts_with('p') {
            self.pending.lock().unwrap().retain(|p| p.key != key);
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

    async fn tokio_sleep(ms: u64) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let _ = tx.send(());
        });
        let _ = rx.await;
    }
}

//! Transfer queue. One job runs at a time (parallel copies to one USB drive are slower),
//! the rest wait in order. Progress and results are pushed to the UI as events.

use crate::engine::{self, Ctx, FileJob, Progress, Stop, StopResult};
use crate::package::{self, Meta};
use crate::steam::{self, Game};
use crate::{now_string, sys};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSpec {
    /// Copy (or resume, or update) a Steam game into the binder.
    Backup { app_id: String, binder: PathBuf },
    /// Copy (or resume, or update) a binder package into a Steam library.
    Restore { package: PathBuf, library: PathBuf },
    /// Byte-for-byte comparison of the installed game and its backup.
    Verify { app_id: String, binder: PathBuf },
    /// Move an installed game to another Steam library on this PC.
    Move { app_id: String, library: PathBuf },
}

#[derive(Debug, Clone, Serialize)]
pub struct QueuedJob {
    pub id: u64,
    pub spec: JobSpec,
    pub title: String,
    pub app_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueSnapshot {
    pub current: Option<QueuedJob>,
    pub pending: Vec<QueuedJob>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    #[serde(flatten)]
    pub progress: Progress,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct JobResult {
    pub id: u64,
    pub kind: String,
    pub app_id: String,
    pub title: String,
    /// "success" | "cancelled" | "failed"
    pub outcome: String,
    pub message: String,
    pub files: u64,
    pub bytes: u64,
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub removed_files: u64,
}

#[derive(Default)]
struct State {
    pending: VecDeque<QueuedJob>,
    current: Option<QueuedJob>,
    next_id: u64,
}

pub struct Queue {
    state: Mutex<State>,
    wake: Condvar,
    cancel: AtomicBool,
}

impl Queue {
    pub fn new() -> Arc<Self> {
        Arc::new(Queue { state: Mutex::new(State::default()), wake: Condvar::new(), cancel: AtomicBool::new(false) })
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        let s = self.state.lock().unwrap();
        QueueSnapshot { current: s.current.clone(), pending: s.pending.iter().cloned().collect() }
    }

    pub fn busy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.current.is_some() || !s.pending.is_empty()
    }

    pub fn enqueue(&self, spec: JobSpec, title: String, app_id: String) -> Result<u64, String> {
        let mut s = self.state.lock().unwrap();
        if s.current.as_ref().map(|j| j.spec == spec).unwrap_or(false) || s.pending.iter().any(|j| j.spec == spec) {
            return Err(format!("{title} is already in the queue."));
        }
        s.next_id += 1;
        let id = s.next_id;
        s.pending.push_back(QueuedJob { id, spec, title, app_id });
        self.wake.notify_all();
        Ok(id)
    }

    /// Cancel the running job, or drop a waiting one.
    pub fn cancel(&self, id: u64) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.current.as_ref().map(|j| j.id == id).unwrap_or(false) {
            self.cancel.store(true, Ordering::Relaxed);
            return true;
        }
        let before = s.pending.len();
        s.pending.retain(|j| j.id != id);
        before != s.pending.len()
    }

    pub fn start_worker(self: &Arc<Self>, app: AppHandle) {
        let queue = Arc::clone(self);
        std::thread::Builder::new()
            .name("transfer-worker".into())
            .spawn(move || loop {
                let job = {
                    let mut s = queue.state.lock().unwrap();
                    while s.pending.is_empty() {
                        s = queue.wake.wait(s).unwrap();
                    }
                    let job = s.pending.pop_front().unwrap();
                    s.current = Some(job.clone());
                    job
                };
                queue.cancel.store(false, Ordering::Relaxed);
                let _ = app.emit("queue", queue.snapshot());

                let result = run_job(&app, &job, &queue.cancel);
                {
                    let mut s = queue.state.lock().unwrap();
                    s.current = None;
                }
                let _ = app.emit("job-finished", &result);
                let _ = app.emit("queue", queue.snapshot());
            })
            .expect("failed to start transfer worker");
    }
}

fn run_job(app: &AppHandle, job: &QueuedJob, cancel: &AtomicBool) -> JobResult {
    let emitter = |p: &Progress| {
        let _ = app.emit("progress", ProgressEvent { id: job.id, app_id: job.app_id.clone(), title: job.title.clone(), progress: p.clone() });
    };
    let ctx = Ctx { cancel, progress: &emitter };
    let mut result = JobResult { id: job.id, app_id: job.app_id.clone(), title: job.title.clone(), ..Default::default() };
    let outcome = match &job.spec {
        JobSpec::Backup { app_id, binder } => {
            result.kind = "backup".into();
            run_backup(app_id, binder, &ctx, &mut result)
        }
        JobSpec::Restore { package, library } => {
            result.kind = "restore".into();
            run_restore(package, library, &ctx, &mut result)
        }
        JobSpec::Verify { app_id, binder } => {
            result.kind = "verify".into();
            run_verify(app_id, binder, &ctx, &mut result)
        }
        JobSpec::Move { app_id, library } => {
            result.kind = "move".into();
            run_move(app_id, library, &ctx, &mut result)
        }
    };
    match outcome {
        Ok(()) => result.outcome = "success".into(),
        Err(Stop::Cancelled) => {
            result.outcome = "cancelled".into();
            result.message = "Stopped. Everything copied so far is kept - start it again to resume.".into();
        }
        Err(Stop::Failed(msg)) => {
            result.outcome = "failed".into();
            result.message = msg;
        }
    }
    result
}

fn find_game(app_id: &str) -> StopResult<Game> {
    steam::scan_all()
        .into_iter()
        .find(|g| g.app_id == app_id)
        .ok_or_else(|| Stop::Failed(format!("App {app_id} is no longer installed in Steam.")))
}

fn inside(child: &Path, parent: &Path) -> bool {
    let c = child.to_string_lossy().to_lowercase();
    let p = parent.to_string_lossy().to_lowercase();
    let p = p.trim_end_matches('\\');
    c == p || c.starts_with(&format!("{p}\\"))
}

fn ensure_space(dest: &Path, needed: u64) -> StopResult<()> {
    if let Some(free) = sys::free_space(dest) {
        // keep a small safety margin for the file system
        if needed + 64 * 1024 * 1024 > free {
            return Err(Stop::Failed(format!(
                "Not enough free space. This needs about {} but the drive only has {} free.",
                crate::format_bytes(needed),
                crate::format_bytes(free)
            )));
        }
    }
    Ok(())
}

fn open_log(path: &Path) -> Box<dyn Write> {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Box::new(std::io::BufWriter::new(f)),
        Err(_) => Box::new(std::io::sink()),
    }
}

fn write_status(path: &Path, text: &str) {
    let _ = fs::write(path, text.replace('\n', "\r\n"));
}

/// Best-effort save-data locations (same set Steam Transfer 1.x used).
fn optional_data_jobs(game: &Game, package: &Path, jobs: &mut Vec<FileJob>) {
    let base = package.join("optional-user-data");
    for (account, dir) in steam::userdata_dirs(&game.app_id) {
        let _ = engine::list_tree(&dir, &base.join("Steam-userdata").join(account).join(&game.app_id), jobs);
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    if let Some(local) = env("LOCALAPPDATA") {
        candidates.push((local.join(&game.install_dir), format!("LocalAppData-{}", game.install_dir)));
        candidates.push((local.join(&game.name), format!("LocalAppData-{}", game.name)));
    }
    if let Some(roaming) = env("APPDATA") {
        candidates.push((roaming.join(&game.install_dir), format!("RoamingAppData-{}", game.install_dir)));
    }
    if let Some(profile) = env("USERPROFILE") {
        candidates.push((profile.join("Documents").join("My Games").join(&game.install_dir), format!("Documents-My Games-{}", game.install_dir)));
    }
    let mut seen: Vec<String> = Vec::new();
    for (dir, label) in candidates {
        let key = dir.to_string_lossy().to_lowercase();
        if seen.contains(&key) || !dir.is_dir() {
            continue;
        }
        seen.push(key);
        let _ = engine::list_tree(&dir, &base.join("matched-locations").join(package::safe_name(&label)), jobs);
    }
}

fn run_backup(app_id: &str, binder: &Path, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    let game = find_game(app_id)?;
    let extra = optional_data_jobs_for(&game, binder);
    backup_game(&game, binder, extra, ctx, result)
}

fn optional_data_jobs_for(game: &Game, binder: &Path) -> Vec<FileJob> {
    let mut jobs = Vec::new();
    optional_data_jobs(game, &package::find_package(binder, game), &mut jobs);
    jobs
}

/// Back up (or resume, or update) one game. `extra_jobs` are save-data files copied alongside.
pub(crate) fn backup_game(game: &Game, binder: &Path, extra_jobs: Vec<FileJob>, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    let game = game.clone();
    let pkg = package::find_package(binder, &game);
    if inside(&pkg, &game.game_path) || inside(&game.game_path, &pkg) {
        return Err(Stop::Failed("Choose a destination outside the game's installation folder.".into()));
    }
    fs::create_dir_all(&pkg).map_err(|e| Stop::Failed(format!("Could not create the backup folder: {e}")))?;

    let status_path = pkg.join(package::STATUS_FILE);
    let error_path = pkg.join("COPY-ERROR.txt");
    let was_complete = package::read_status(&status_path) == Some(package::Status::Complete);
    write_status(&status_path, &format!("IN PROGRESS\nStarted: {}\n", now_string()));
    let _ = fs::write(&error_path, "");
    let mut log = open_log(&pkg.join("COPY-LOG.txt"));

    let outcome = (|| -> StopResult<()> {
        let game_dest = pkg.join("steamapps").join("common").join(&game.install_dir);
        let mut game_jobs = Vec::new();
        engine::list_tree(&game.game_path, &game_dest, &mut game_jobs).map_err(|e| Stop::Failed(format!("Could not read the game folder: {e}")))?;

        let _ = writeln!(log, "============================================================");
        let _ = writeln!(log, "Steam Transfer {} {} started: {}", env!("CARGO_PKG_VERSION"), if was_complete { "update" } else { "backup" }, now_string());
        let _ = writeln!(log, "Game: {} (App {}) build {}", game.name, game.app_id, game.build_id);
        let _ = writeln!(log, "Source: {}", game.game_path.display());
        let _ = writeln!(log, "Package: {}", pkg.display());
        if !game.fully_installed() {
            let _ = writeln!(log, "WARNING: Steam reports this game has a pending update (StateFlags {}).", game.state_flags);
        }

        engine::clean_partials(&pkg);
        // Files the game no longer has (removed by a Steam update) are removed from the backup.
        let removed = engine::mirror_deletions(&game_dest, &game_jobs).map_err(|e| Stop::Failed(format!("Could not remove outdated files: {e}")))?;
        for rel in &removed {
            let _ = writeln!(log, "DELETE  {rel}");
        }
        result.removed_files = removed.len() as u64;

        let mut all = game_jobs.clone();
        all.extend(extra_jobs.clone());
        let _ = writeln!(log, "Files: {}; bytes: {}", all.len(), all.iter().map(|j| j.len).sum::<u64>());
        ensure_space(&pkg, engine::bytes_needed(&all))?;

        let stats = engine::copy_jobs(&all, ctx, if was_complete { "Updating backup" } else { "Copying" }, &mut log)?;
        result.copied_files = stats.copied_files;
        result.copied_bytes = stats.copied_bytes;

        ctx.check()?;
        (ctx.progress)(&Progress { stage: "Verifying".into(), ..Default::default() });
        engine::verify_sizes(&all)?;
        let (src_files, src_bytes) = (game_jobs.len() as u64, game_jobs.iter().map(|j| j.len).sum::<u64>());
        let (dst_files, dst_bytes) = engine::measure(&game_dest);
        if src_files != dst_files || src_bytes != dst_bytes {
            return Err(Stop::Failed(format!(
                "Verification failed: the game has {src_files} files / {src_bytes} bytes but the backup has {dst_files} files / {dst_bytes} bytes."
            )));
        }

        // Manifest last: its presence + SUCCESS status is what marks a finished package.
        let manifest_dest = pkg.join("steamapps").join(game.manifest_path.file_name().unwrap_or_default());
        fs::copy(&game.manifest_path, &manifest_dest).map_err(|e| Stop::Failed(format!("Could not copy the Steam manifest: {e}")))?;
        if let Some(art) = &game.art {
            let _ = fs::copy(art, pkg.join(package::COVER_FILE));
        }
        let readme = pkg.join(format!("README - Restore {}.txt", package::safe_name(&game.name)));
        let _ = fs::write(readme, package::readme_text(&game, src_bytes));
        let _ = package::write_meta(
            &pkg,
            &Meta {
                format: 2,
                app_id: game.app_id.clone(),
                name: game.name.clone(),
                install_dir: game.install_dir.clone(),
                build_id: game.build_id.clone(),
                files: src_files,
                bytes: src_bytes,
                completed_at: now_string(),
                app_version: env!("CARGO_PKG_VERSION").into(),
            },
        );
        result.files = src_files;
        result.bytes = src_bytes;
        let _ = writeln!(log, "SUCCESS at {}  copied {} files ({} bytes), removed {}", now_string(), stats.copied_files, stats.copied_bytes, removed.len());
        let _ = writeln!(log, "Verified game files: {src_files}; verified game bytes: {src_bytes}");
        write_status(
            &status_path,
            &format!(
                "SUCCESS\nCompleted: {}\nGame: {}\nApp ID: {}\nBuild: {}\nVerified files: {}\nVerified bytes: {}\n",
                now_string(),
                game.name,
                game.app_id,
                game.build_id,
                src_files,
                src_bytes
            ),
        );
        result.message = if was_complete {
            if stats.copied_files == 0 && removed.is_empty() {
                "Backup was already up to date.".into()
            } else {
                format!("Backup updated: {} changed files copied, {} removed.", stats.copied_files, removed.len())
            }
        } else {
            "Backup complete and verified.".into()
        };
        Ok(())
    })();

    match &outcome {
        Err(Stop::Cancelled) => {
            let _ = writeln!(log, "CANCELLED at {}", now_string());
            write_status(&status_path, "CANCELLED\nYou can safely resume from the app later.\n");
        }
        Err(Stop::Failed(msg)) => {
            let _ = writeln!(log, "FAILED at {}: {msg}", now_string());
            let _ = fs::write(&error_path, msg);
            write_status(&status_path, &format!("FAILED\n{msg}\n"));
        }
        Ok(()) => {}
    }
    let _ = log.flush();
    outcome
}

/// Marker in the target library while a restore is unfinished, so the UI can offer "Resume".
pub fn restore_marker(library: &Path, app_id: &str) -> PathBuf {
    library.join("steamapps").join(format!("steamtransfer-restore-{app_id}.inprogress"))
}

fn run_restore(package_path: &Path, library: &Path, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    if sys::steam_running() {
        return Err(Stop::Failed("Steam is running. Exit Steam completely (including the tray icon) before restoring, so it can't overwrite the game's manifest.".into()));
    }
    restore_package(package_path, library, ctx, result)
}

/// Restore (or resume, or update) one backup into a Steam library.
pub(crate) fn restore_package(package_path: &Path, library: &Path, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    let pkg = package::read_package(package_path).ok_or_else(|| Stop::Failed("That backup could not be read.".into()))?;
    let game = pkg.game.clone().ok_or_else(|| Stop::Failed("The backup is missing its Steam manifest. Finish or update the backup first.".into()))?;
    if pkg.status != package::Status::Complete {
        return Err(Stop::Failed("This backup is unfinished. Resume it on the original PC first.".into()));
    }
    let steamapps = library.join("steamapps");
    if !steamapps.is_dir() {
        return Err(Stop::Failed(format!("{} is not a Steam library (no steamapps folder).", library.display())));
    }
    let target = steamapps.join("common").join(&game.install_dir);
    if inside(&target, package_path) || inside(package_path, &target) {
        return Err(Stop::Failed("The Steam library must be separate from the backup.".into()));
    }

    let marker = restore_marker(library, &game.app_id);
    let _ = fs::write(&marker, now_string());
    let status_path = package_path.join("RESTORE-STATUS.txt");
    write_status(&status_path, &format!("IN PROGRESS\nStarted: {}\nTarget library: {}\n", now_string(), library.display()));
    let mut log = open_log(&package_path.join("RESTORE-LOG.txt"));

    let outcome = (|| -> StopResult<()> {
        fs::create_dir_all(&target).map_err(|e| Stop::Failed(format!("Could not create the game folder: {e}")))?;
        engine::clean_partials(&target);
        let mut jobs = Vec::new();
        engine::list_tree(&game.game_path, &target, &mut jobs).map_err(|e| Stop::Failed(format!("Could not read the backup: {e}")))?;
        let _ = writeln!(log, "============================================================");
        let _ = writeln!(log, "Restore started: {}", now_string());
        let _ = writeln!(log, "Game: {} (App {}) build {}", game.name, game.app_id, game.build_id);
        let _ = writeln!(log, "Target library: {}", library.display());
        let _ = writeln!(log, "Files: {}; bytes: {}", jobs.len(), jobs.iter().map(|j| j.len).sum::<u64>());
        ensure_space(&target, engine::bytes_needed(&jobs))?;

        let stats = engine::copy_jobs(&jobs, ctx, "Restoring", &mut log)?;
        result.copied_files = stats.copied_files;
        result.copied_bytes = stats.copied_bytes;
        ctx.check()?;
        (ctx.progress)(&Progress { stage: "Verifying".into(), ..Default::default() });
        engine::verify_sizes(&jobs)?;

        let manifest_dest = steamapps.join(game.manifest_path.file_name().unwrap_or_default());
        fs::copy(&game.manifest_path, &manifest_dest).map_err(|e| Stop::Failed(format!("Could not install the Steam manifest: {e}")))?;
        let _ = fs::remove_file(&marker);

        let bytes: u64 = jobs.iter().map(|j| j.len).sum();
        result.files = jobs.len() as u64;
        result.bytes = bytes;
        let _ = writeln!(log, "SUCCESS at {}  copied {} files ({} bytes)", now_string(), stats.copied_files, stats.copied_bytes);
        write_status(
            &status_path,
            &format!(
                "SUCCESS\nCompleted: {}\nGame: {}\nApp ID: {}\nTarget library: {}\nVerified files: {}\nVerified bytes: {}\n",
                now_string(),
                game.name,
                game.app_id,
                library.display(),
                jobs.len(),
                bytes
            ),
        );
        result.message = if stats.copied_files == 0 {
            "Already installed and up to date. Start Steam to play.".into()
        } else {
            "Restored and verified. Start Steam - it will pick the game up.".into()
        };
        Ok(())
    })();

    match &outcome {
        Err(Stop::Cancelled) => {
            let _ = writeln!(log, "CANCELLED at {}", now_string());
            write_status(&status_path, "CANCELLED\nThe restore can be resumed safely.\n");
        }
        Err(Stop::Failed(msg)) => {
            let _ = writeln!(log, "FAILED at {}: {msg}", now_string());
            write_status(&status_path, &format!("FAILED\n{msg}\n"));
        }
        Ok(()) => {}
    }
    let _ = log.flush();
    outcome
}

/// Marker in the target library while a move is unfinished. Until the move completes the game
/// still belongs to its original library, so Steam keeps working whatever happens.
pub fn move_marker(library: &Path, app_id: &str) -> PathBuf {
    library.join("steamapps").join(format!("steamtransfer-move-{app_id}.inprogress"))
}

fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_uppercase());
    root(a).is_some() && root(a) == root(b)
}

/// Everything Steam keeps for one game inside a library: the game folder (required),
/// its Workshop items and its shader cache (moved so Steam doesn't download/rebuild them).
fn move_parts(game: &Game, target: &Path) -> Vec<(PathBuf, PathBuf)> {
    let src = game.library_root.join("steamapps");
    let dst = target.join("steamapps");
    let id = &game.app_id;
    vec![
        (game.game_path.clone(), dst.join("common").join(&game.install_dir)),
        (src.join("workshop").join("content").join(id), dst.join("workshop").join("content").join(id)),
        (src.join("shadercache").join(id), dst.join("shadercache").join(id)),
    ]
}

fn move_log() -> Box<dyn Write> {
    match std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("SteamTransfer")) {
        Some(dir) if fs::create_dir_all(&dir).is_ok() => open_log(&dir.join("move-log.txt")),
        _ => Box::new(std::io::sink()),
    }
}

fn run_move(app_id: &str, library: &Path, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    if sys::steam_running() {
        return Err(Stop::Failed("Steam is running. Exit Steam completely (including the tray icon) before moving a game, so it doesn't keep using the old location.".into()));
    }
    let game = find_game(app_id)?;
    move_game(&game, library, false, ctx, result)
}

/// Move a game to another Steam library. Order matters so Steam is never left without a valid install:
/// 1. copy (or rename, on the same drive) the files into the target library and verify them,
/// 2. put the manifest in the target library, 3. remove the old manifest, 4. delete the old files.
pub(crate) fn move_game(game: &Game, target: &Path, force_copy: bool, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    if !game.fully_installed() {
        return Err(Stop::Failed("Steam has an update or download waiting for this game. Let Steam finish it first, then move the game.".into()));
    }
    let src_apps = game.library_root.join("steamapps");
    let dst_apps = target.join("steamapps");
    if !dst_apps.is_dir() {
        return Err(Stop::Failed(format!("{} is not a Steam library. Add it in Steam > Settings > Storage first.", target.display())));
    }
    if inside(&game.library_root, target) && inside(target, &game.library_root) {
        return Err(Stop::Failed("The game is already in that library.".into()));
    }
    let manifest_name = game.manifest_path.file_name().unwrap_or_default().to_os_string();
    let dst_manifest = dst_apps.join(&manifest_name);
    let marker = move_marker(target, &game.app_id);
    let resuming = marker.exists();
    let parts = move_parts(game, target);
    let dst_game = parts[0].1.clone();
    if dst_manifest.exists() && !resuming {
        return Err(Stop::Failed("That library already has a Steam install record for this game. Uninstall it there in Steam first.".into()));
    }
    let dst_has_files = fs::read_dir(&dst_game).map(|mut d| d.next().is_some()).unwrap_or(false);
    if dst_has_files && !resuming {
        return Err(Stop::Failed(format!(
            "{} already exists. Rename or delete that folder first so the two copies don't get mixed up.",
            dst_game.display()
        )));
    }
    if inside(&dst_game, &game.game_path) || inside(&game.game_path, &dst_game) {
        return Err(Stop::Failed("The target folder overlaps the game's current folder.".into()));
    }

    let mut log = move_log();
    let _ = writeln!(log, "============================================================");
    let _ = writeln!(log, "Move started: {}  {} (App {}) build {}", now_string(), game.name, game.app_id, game.build_id);
    let _ = writeln!(log, "From: {}", game.library_root.display());
    let _ = writeln!(log, "To:   {}", target.display());
    let _ = fs::write(&marker, now_string());

    let rename = !force_copy && same_volume(&game.library_root, target) && !dst_game.exists();
    let outcome = (|| -> StopResult<()> {
        if rename {
            // Same drive: moving folders is instant and needs no extra space.
            (ctx.progress)(&Progress { stage: "Moving".into(), file: game.install_dir.clone(), ..Default::default() });
            fs::create_dir_all(dst_apps.join("common"))?;
            fs::rename(&game.game_path, &dst_game).map_err(|e| Stop::Failed(format!("Could not move the game folder: {e}")))?;
            for (src, dst) in &parts[1..] {
                if src.is_dir() && !dst.exists() {
                    if let Some(parent) = dst.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::rename(src, dst);
                }
            }
            let (files, bytes) = engine::measure(&dst_game);
            result.files = files;
            result.bytes = bytes;
            let _ = writeln!(log, "RENAMED on the same drive: {files} files");
        } else {
            let mut game_jobs = Vec::new();
            engine::list_tree(&game.game_path, &dst_game, &mut game_jobs).map_err(|e| Stop::Failed(format!("Could not read the game folder: {e}")))?;
            let mut extra = Vec::new();
            for (src, dst) in &parts[1..] {
                let _ = engine::list_tree(src, dst, &mut extra);
            }
            if dst_game.is_dir() {
                engine::clean_partials(&dst_game);
                // only our own unfinished copy can be here (checked above), so make it an exact mirror
                engine::mirror_deletions(&dst_game, &game_jobs)?;
            }
            let mut all = game_jobs.clone();
            all.extend(extra);
            ensure_space(&dst_apps, engine::bytes_needed(&all))?;
            let stats = engine::copy_jobs(&all, ctx, "Moving", &mut log)?;
            ctx.check()?;
            (ctx.progress)(&Progress { stage: "Verifying".into(), ..Default::default() });
            engine::verify_sizes(&all)?;
            let src_files = game_jobs.len() as u64;
            let src_bytes: u64 = game_jobs.iter().map(|j| j.len).sum();
            if engine::measure(&dst_game) != (src_files, src_bytes) {
                return Err(Stop::Failed("Verification failed: the copy doesn't match the original. Nothing was removed.".into()));
            }
            result.copied_files = stats.copied_files;
            result.copied_bytes = stats.copied_bytes;
            result.files = src_files;
            result.bytes = src_bytes;
        }

        // Hand the game over to the target library.
        fs::copy(&game.manifest_path, &dst_manifest).map_err(|e| Stop::Failed(format!("Could not write the Steam manifest: {e}")))?;
        let workshop_acf = format!("appworkshop_{}.acf", game.app_id);
        let src_ws = src_apps.join("workshop").join(&workshop_acf);
        if src_ws.is_file() {
            let _ = fs::create_dir_all(dst_apps.join("workshop"));
            let _ = fs::copy(&src_ws, dst_apps.join("workshop").join(&workshop_acf));
        }
        if let Err(e) = engine::remove_file_force(&game.manifest_path) {
            // Never leave two libraries claiming the same game.
            let _ = fs::remove_file(&dst_manifest);
            return Err(Stop::Failed(format!("Could not remove the old Steam manifest ({e}). The game stays in its original library.")));
        }
        let _ = engine::remove_file_force(&src_ws);
        let _ = fs::remove_file(&marker);
        let _ = writeln!(log, "HANDED OVER at {}", now_string());

        if !rename {
            (ctx.progress)(&Progress { stage: "Removing the old copy".into(), ..Default::default() });
            let mut leftovers = Vec::new();
            for (src, _) in &parts {
                if let Err(e) = engine::delete_tree(src) {
                    leftovers.push(format!("{} ({e})", src.display()));
                }
            }
            if !leftovers.is_empty() {
                let _ = writeln!(log, "LEFTOVERS: {}", leftovers.join("; "));
                result.message = format!(
                    "Moved to {}. Some old files couldn't be deleted - you can remove them by hand: {}",
                    target.display(),
                    leftovers.join("; ")
                );
                return Ok(());
            }
        }
        result.message = format!("Moved to {}. Start Steam - it will find the game there.", target.display());
        Ok(())
    })();

    match &outcome {
        Ok(()) => {
            let _ = writeln!(log, "SUCCESS at {}", now_string());
        }
        Err(Stop::Cancelled) => {
            let _ = writeln!(log, "CANCELLED at {} - the game is still in its original library", now_string());
        }
        Err(Stop::Failed(msg)) => {
            let _ = writeln!(log, "FAILED at {}: {msg}", now_string());
        }
    }
    let _ = log.flush();
    outcome
}

fn run_verify(app_id: &str, binder: &Path, ctx: &Ctx, result: &mut JobResult) -> StopResult<()> {
    let game = find_game(app_id)?;
    let pkg = package::find_package(binder, &game);
    let game_dest = pkg.join("steamapps").join("common").join(&game.install_dir);
    if !game_dest.is_dir() {
        return Err(Stop::Failed("There is no backup of this game yet.".into()));
    }
    let mut jobs = Vec::new();
    engine::list_tree(&game.game_path, &game_dest, &mut jobs)?;
    let bad = engine::deep_compare(&jobs, ctx)?;
    let mut extra = Vec::new();
    engine::list_tree(&game_dest, &game_dest, &mut extra)?;
    let source_files: std::collections::HashSet<String> = jobs.iter().map(|j| j.rel.to_lowercase()).collect();
    let extras = extra.iter().filter(|e| !source_files.contains(&e.rel.to_lowercase())).count();
    result.files = jobs.len() as u64;
    result.bytes = jobs.iter().map(|j| j.len).sum();
    let mut log = open_log(&pkg.join("COPY-LOG.txt"));
    if bad.is_empty() && extras == 0 {
        let _ = writeln!(log, "DEEP VERIFY OK at {}: {} files identical", now_string(), jobs.len());
        result.message = format!("Every byte matches - {} files identical.", jobs.len());
        Ok(())
    } else {
        let _ = writeln!(log, "DEEP VERIFY FOUND DIFFERENCES at {}:", now_string());
        for rel in &bad {
            let _ = writeln!(log, "  DIFFERENT  {rel}");
        }
        let sample: Vec<&str> = bad.iter().take(3).map(|s| s.as_str()).collect();
        Err(Stop::Failed(format!(
            "{} file(s) differ from the installed game{}. Run Update to fix them.",
            bad.len() + extras,
            if sample.is_empty() { String::new() } else { format!(" (e.g. {})", sample.join(", ")) }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Progress;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn manifest(library: &Path, build: &str) -> PathBuf {
        let path = library.join("steamapps").join("appmanifest_424242.acf");
        write(
            &path,
            format!("\"AppState\"\n{{\n\t\"appid\"\t\t\"424242\"\n\t\"name\"\t\t\"Test: Game\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"Test Game\"\n\t\"SizeOnDisk\"\t\t\"100\"\n\t\"buildid\"\t\t\"{build}\"\n}}\n").as_bytes(),
        );
        path
    }

    fn game(library: &Path) -> Game {
        crate::steam::read_manifest(&library.join("steamapps").join("appmanifest_424242.acf"), library).unwrap()
    }

    #[test]
    fn backup_update_and_restore_round_trip() {
        let root = std::env::temp_dir().join(format!("steam-transfer-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let pc = root.join("pc-library");
        let drive = root.join("external").join(package::BINDER_DIR);
        let new_pc = root.join("new-pc-library");
        fs::create_dir_all(new_pc.join("steamapps")).unwrap();

        let cancel = AtomicBool::new(false);
        let progress = |_: &Progress| {};
        let ctx = Ctx { cancel: &cancel, progress: &progress };

        // 1. back up build 100
        manifest(&pc, "100");
        let common = pc.join("steamapps").join("common").join("Test Game");
        write(&common.join("game.exe"), b"v1");
        write(&common.join("data").join("old.pak"), &vec![1u8; 5000]);
        let saves = vec![]; // no save data in the fixture
        let mut r = JobResult::default();
        backup_game(&game(&pc), &drive, saves, &ctx, &mut r).ok().unwrap();
        let pkg_path = drive.join("Test_ Game (App 424242)");
        let pkg = package::read_package(&pkg_path).unwrap();
        assert_eq!(pkg.status, package::Status::Complete);
        assert_eq!(pkg.build_id, "100");
        assert_eq!(r.copied_files, 2);

        // 2. restore onto the "new PC"
        let mut r = JobResult::default();
        restore_package(&pkg_path, &new_pc, &ctx, &mut r).ok().unwrap();
        let installed = new_pc.join("steamapps").join("common").join("Test Game");
        assert_eq!(fs::read(installed.join("game.exe")).unwrap(), b"v1");
        assert!(new_pc.join("steamapps").join("appmanifest_424242.acf").is_file());
        assert!(!restore_marker(&new_pc, "424242").exists());

        // 3. Steam updates the game on the first PC to build 200
        std::thread::sleep(std::time::Duration::from_millis(50));
        manifest(&pc, "200");
        write(&common.join("game.exe"), b"v2 bigger");
        fs::remove_file(common.join("data").join("old.pak")).unwrap();
        write(&common.join("data").join("new.pak"), b"new");

        // 4. push the update to the drive: only changes copied, deleted file removed
        let mut r = JobResult::default();
        backup_game(&game(&pc), &drive, vec![], &ctx, &mut r).ok().unwrap();
        assert_eq!(r.copied_files, 2);
        assert_eq!(r.removed_files, 1);
        assert_eq!(package::read_package(&pkg_path).unwrap().build_id, "200");

        // 5. apply the update on the new PC
        let mut r = JobResult::default();
        restore_package(&pkg_path, &new_pc, &ctx, &mut r).ok().unwrap();
        assert_eq!(r.copied_files, 2);
        assert_eq!(fs::read(installed.join("game.exe")).unwrap(), b"v2 bigger");
        let restored_manifest = fs::read_to_string(new_pc.join("steamapps").join("appmanifest_424242.acf")).unwrap();
        assert!(restored_manifest.contains("\"200\""));

        // 6. an interrupted backup is reported as unfinished and refuses to restore
        write(&pkg_path.join(package::STATUS_FILE), b"IN PROGRESS\r\n");
        assert_eq!(package::read_package(&pkg_path).unwrap().status, package::Status::Incomplete);
        let mut r = JobResult::default();
        assert!(matches!(restore_package(&pkg_path, &new_pc, &ctx, &mut r), Err(Stop::Failed(_))));

        let _ = fs::remove_dir_all(&root);
    }

    fn library_with_game(root: &Path, name: &str) -> PathBuf {
        let lib = root.join(name);
        manifest(&lib, "100");
        let apps = lib.join("steamapps");
        write(&apps.join("common").join("Test Game").join("game.exe"), b"exe");
        write(&apps.join("common").join("Test Game").join("data").join("big.pak"), &vec![3u8; 20_000]);
        write(&apps.join("workshop").join("content").join("424242").join("mod1").join("mod.bin"), b"mod");
        write(&apps.join("workshop").join("appworkshop_424242.acf"), b"\"AppWorkshop\" {}");
        write(&apps.join("shadercache").join("424242").join("cache.bin"), b"shader");
        lib
    }

    fn assert_moved(from: &Path, to: &Path) {
        let src = from.join("steamapps");
        let dst = to.join("steamapps");
        // the game, its manifest, workshop items and shader cache live in the new library...
        assert_eq!(fs::read(dst.join("common").join("Test Game").join("game.exe")).unwrap(), b"exe");
        assert_eq!(fs::metadata(dst.join("common").join("Test Game").join("data").join("big.pak")).unwrap().len(), 20_000);
        assert!(dst.join("appmanifest_424242.acf").is_file());
        assert!(dst.join("workshop").join("content").join("424242").join("mod1").join("mod.bin").is_file());
        assert!(dst.join("workshop").join("appworkshop_424242.acf").is_file());
        assert!(dst.join("shadercache").join("424242").join("cache.bin").is_file());
        assert!(!move_marker(to, "424242").exists());
        // ...and nothing is left behind, so Steam sees exactly one install
        assert!(!src.join("appmanifest_424242.acf").exists());
        assert!(!src.join("common").join("Test Game").exists());
        assert!(!src.join("workshop").join("content").join("424242").exists());
        assert!(!src.join("workshop").join("appworkshop_424242.acf").exists());
        assert!(!src.join("shadercache").join("424242").exists());
        // Steam reads the moved manifest exactly as before
        let moved = crate::steam::scan_library(to);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].app_id, "424242");
        assert!(crate::steam::scan_library(from).is_empty());
    }

    fn ctx_parts() -> (AtomicBool, impl Fn(&Progress) + Sync) {
        (AtomicBool::new(false), |_: &Progress| {})
    }

    #[test]
    fn move_between_drives_copies_verifies_then_removes() {
        let root = std::env::temp_dir().join(format!("steam-transfer-move-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let from = library_with_game(&root, "lib-c");
        let to = root.join("lib-e");
        fs::create_dir_all(to.join("steamapps")).unwrap();
        let (cancel, progress) = ctx_parts();
        let ctx = Ctx { cancel: &cancel, progress: &progress };

        let mut r = JobResult::default();
        move_game(&game(&from), &to, true, &ctx, &mut r).ok().unwrap();
        assert_eq!(r.files, 2);
        assert_moved(&from, &to);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_on_same_drive_renames_instantly() {
        let root = std::env::temp_dir().join(format!("steam-transfer-move-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let from = library_with_game(&root, "lib-a");
        let to = root.join("lib-b");
        fs::create_dir_all(to.join("steamapps")).unwrap();
        let (cancel, progress) = ctx_parts();
        let ctx = Ctx { cancel: &cancel, progress: &progress };

        let mut r = JobResult::default();
        move_game(&game(&from), &to, false, &ctx, &mut r).ok().unwrap();
        assert_eq!(r.copied_files, 0, "same drive must rename, not copy");
        assert_moved(&from, &to);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn interrupted_move_keeps_the_original_and_resumes() {
        let root = std::env::temp_dir().join(format!("steam-transfer-move-resume-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let from = library_with_game(&root, "lib-c");
        let to = root.join("lib-e");
        fs::create_dir_all(to.join("steamapps")).unwrap();

        // a cancelled move leaves the original untouched and Steam still sees it there
        let cancel = AtomicBool::new(true);
        let progress = |_: &Progress| {};
        let ctx = Ctx { cancel: &cancel, progress: &progress };
        let mut r = JobResult::default();
        assert!(matches!(move_game(&game(&from), &to, true, &ctx, &mut r), Err(Stop::Cancelled)));
        assert!(from.join("steamapps").join("appmanifest_424242.acf").is_file());
        assert!(from.join("steamapps").join("common").join("Test Game").join("game.exe").is_file());
        assert!(!to.join("steamapps").join("appmanifest_424242.acf").exists());
        assert!(move_marker(&to, "424242").exists());

        // simulate a half-copied file from the interruption, then resume
        write(&to.join("steamapps").join("common").join("Test Game").join("game.exe"), b"exe");
        write(&to.join("steamapps").join("common").join("Test Game").join("data").join("big.pak.steamtransfer-part"), b"half");
        let (cancel, progress) = ctx_parts();
        let ctx = Ctx { cancel: &cancel, progress: &progress };
        let mut r = JobResult::default();
        move_game(&game(&from), &to, true, &ctx, &mut r).ok().unwrap();
        assert_moved(&from, &to);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn move_refuses_to_mix_with_an_existing_folder() {
        let root = std::env::temp_dir().join(format!("steam-transfer-move-refuse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let from = library_with_game(&root, "lib-c");
        let to = root.join("lib-e");
        write(&to.join("steamapps").join("common").join("Test Game").join("old-save.dat"), b"keep me");
        let (cancel, progress) = ctx_parts();
        let ctx = Ctx { cancel: &cancel, progress: &progress };
        let mut r = JobResult::default();
        assert!(matches!(move_game(&game(&from), &to, true, &ctx, &mut r), Err(Stop::Failed(_))));
        assert!(to.join("steamapps").join("common").join("Test Game").join("old-save.dat").is_file());
        assert!(from.join("steamapps").join("appmanifest_424242.acf").is_file());
        let _ = fs::remove_dir_all(&root);
    }
}

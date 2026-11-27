mod covers;
mod engine;
mod jobs;
mod package;
mod settings;
mod steam;
mod sys;
mod torrent;
mod vdf;
mod web_library;

use jobs::{JobSpec, Queue, QueueSnapshot};
use serde::Serialize;
use settings::Settings;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

pub struct AppState {
    queue: Arc<Queue>,
    /// Art key ("lib-<id>" / "pkg-<id>") -> image file, served at http://art.localhost/<key>
    art: covers::ArtMap,
    settings: Mutex<Settings>,
    torrents: Arc<torrent::Torrents>,
    covers: Arc<covers::Covers>,
}

type CmdResult<T> = Result<T, String>;

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(windows)]
pub fn now_string() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(not(windows))]
pub fn now_string() -> String {
    format!("{:?}", std::time::SystemTime::now())
}

fn art_url(app: &AppHandle, key: String, path: Option<&PathBuf>) -> Option<String> {
    let path = path?;
    app.state::<AppState>().art.lock().unwrap().insert(key.clone(), path.clone());
    Some(format!("http://art.localhost/{key}"))
}

// ---------------------------------------------------------------------------------------------
// DTOs

#[derive(Serialize)]
struct GameDto {
    #[serde(flatten)]
    game: steam::Game,
    art_url: Option<String>,
    fully_installed: bool,
}

#[derive(Serialize)]
struct PackageDto {
    #[serde(flatten)]
    package: package::Package,
    art_url: Option<String>,
}

#[derive(Serialize)]
struct BinderDto {
    root: PathBuf,
    exists: bool,
    free_bytes: Option<u64>,
    packages: Vec<PackageDto>,
}

#[derive(Serialize)]
struct DriveDto {
    path: PathBuf,
    free_bytes: Option<u64>,
}

#[derive(Serialize, Default)]
struct InstalledDto {
    build_id: String,
    /// an unfinished restore into this library
    partial: bool,
    /// an unfinished move into this library
    moving: bool,
}

#[derive(Serialize)]
struct UpdateDto {
    version: String,
    notes: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Commands

#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, state: tauri::State<'_, AppState>, settings: Settings) -> CmdResult<()> {
    *state.settings.lock().unwrap() = settings.clone();
    settings::save(&app, &settings)
}

#[tauri::command]
async fn pick_folder(app: AppHandle, title: String, start: Option<String>) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let mut dialog = app.dialog().file().set_title(title);
    if let Some(start) = start.filter(|s| Path::new(s).is_dir()) {
        dialog = dialog.set_directory(start);
    }
    Ok(dialog
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
async fn scan_library(app: AppHandle) -> CmdResult<Vec<GameDto>> {
    let games = tauri::async_runtime::spawn_blocking(steam::scan_all).await.map_err(|e| e.to_string())?;
    Ok(games
        .into_iter()
        .map(|g| GameDto {
            art_url: art_url(&app, format!("lib-{}", g.app_id), g.art.as_ref()),
            fully_installed: g.fully_installed(),
            game: g,
        })
        .collect())
}

#[tauri::command]
async fn scan_binder(app: AppHandle, path: String) -> CmdResult<BinderDto> {
    let root = package::binder_root(Path::new(&path));
    let scan_root = root.clone();
    let packages = tauri::async_runtime::spawn_blocking(move || package::scan_binder(&scan_root)).await.map_err(|e| e.to_string())?;
    Ok(BinderDto {
        exists: root.is_dir(),
        free_bytes: sys::free_space(&root),
        packages: packages
            .into_iter()
            .map(|p| PackageDto { art_url: art_url(&app, format!("pkg-{}", p.app_id), p.art.as_ref()), package: p })
            .collect(),
        root,
    })
}

/// Drives that already contain a "Steam Transfer" folder.
#[tauri::command]
async fn find_binders() -> CmdResult<Vec<DriveDto>> {
    tauri::async_runtime::spawn_blocking(|| {
        sys::drive_roots()
            .into_iter()
            .map(|d| d.join(package::BINDER_DIR))
            .filter(|b| b.is_dir())
            .map(|b| DriveDto { free_bytes: sys::free_space(&b), path: b })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct LibraryDto {
    path: PathBuf,
    free_bytes: Option<u64>,
    /// false = found on a drive but Steam doesn't list it yet (it gets added on first use)
    registered: bool,
}

/// Every Steam library on every drive.
#[tauri::command]
async fn list_libraries() -> CmdResult<Vec<LibraryDto>> {
    tauri::async_runtime::spawn_blocking(|| {
        steam::all_libraries()
            .into_iter()
            .map(|l| LibraryDto { free_bytes: sys::free_space(&l.path), registered: l.registered, path: l.path })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct CreatedLibrary {
    path: PathBuf,
    note: String,
}

/// Create a Steam library on the chosen drive/folder and add it to Steam.
#[tauri::command]
async fn create_library(path: String) -> CmdResult<CreatedLibrary> {
    tauri::async_runtime::spawn_blocking(move || steam::create_library(Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
        .map(|(path, note)| CreatedLibrary { path, note })
}

#[tauri::command]
fn detect_library(path: String) -> Option<PathBuf> {
    steam::detect_library(Path::new(&path))
}

/// What is already installed in a target library (for Restore badges).
#[tauri::command]
async fn library_state(library: String) -> CmdResult<HashMap<String, InstalledDto>> {
    tauri::async_runtime::spawn_blocking(move || {
        let library = PathBuf::from(library);
        let mut map: HashMap<String, InstalledDto> = steam::scan_library(&library)
            .into_iter()
            .map(|g| (g.app_id.clone(), InstalledDto { build_id: g.build_id, ..Default::default() }))
            .collect();
        if let Ok(entries) = std::fs::read_dir(library.join("steamapps")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_prefix("steamtransfer-restore-").and_then(|n| n.strip_suffix(".inprogress")) {
                    map.entry(id.to_string()).or_default().partial = true;
                }
                if let Some(id) = name.strip_prefix("steamtransfer-move-").and_then(|n| n.strip_suffix(".inprogress")) {
                    map.entry(id.to_string()).or_default().moving = true;
                }
            }
        }
        map
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn steam_running() -> bool {
    sys::steam_running()
}

#[tauri::command]
fn enqueue(state: tauri::State<'_, AppState>, app: AppHandle, spec: JobSpec, title: String, app_id: String) -> CmdResult<u64> {
    let id = state.queue.enqueue(spec, title, app_id)?;
    let _ = app.emit("queue", state.queue.snapshot());
    Ok(id)
}

#[tauri::command]
fn cancel_job(state: tauri::State<'_, AppState>, app: AppHandle, id: u64) -> bool {
    let ok = state.queue.cancel(id);
    let _ = app.emit("queue", state.queue.snapshot());
    ok
}

#[tauri::command]
fn queue_state(state: tauri::State<'_, AppState>) -> QueueSnapshot {
    state.queue.snapshot()
}

#[tauri::command]
fn open_folder(path: String) -> CmdResult<()> {
    let p = PathBuf::from(&path);
    let target = if p.exists() { p } else { p.parent().map(Path::to_path_buf).unwrap_or(p) };
    sys::open_in_explorer(&target).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Torrents

#[tauri::command]
fn torrent_default_dir(state: tauri::State<'_, AppState>) -> String {
    state.torrents.default_dir().to_string_lossy().into_owned()
}

#[tauri::command]
fn torrent_add(state: tauri::State<'_, AppState>, link: String, folder: String) -> CmdResult<String> {
    state.torrents.add(torrent::Source::Link(link.trim().to_string()), PathBuf::from(folder))
}

#[tauri::command]
async fn pick_torrent_file(app: AppHandle) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    Ok(app
        .dialog()
        .file()
        .set_title("Open a .torrent file")
        .add_filter("Torrent files", &["torrent"])
        .blocking_pick_file()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
fn torrent_add_file(state: tauri::State<'_, AppState>, path: String, folder: String) -> CmdResult<String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("Could not read the torrent file: {e}"))?;
    state.torrents.add(torrent::Source::File(bytes), PathBuf::from(folder))
}

#[tauri::command]
async fn torrent_list(app: AppHandle) -> CmdResult<Vec<torrent::TorrentDto>> {
    let torrents = app.state::<AppState>().torrents.clone();
    Ok(torrents.list().await)
}

#[tauri::command]
async fn torrent_pause(app: AppHandle, key: String) -> CmdResult<()> {
    let torrents = app.state::<AppState>().torrents.clone();
    torrents.pause(&key).await
}

#[tauri::command]
async fn torrent_resume(app: AppHandle, key: String) -> CmdResult<()> {
    let torrents = app.state::<AppState>().torrents.clone();
    torrents.resume(&key).await
}

/// Download / upload limits in KB/s (0 = unlimited). Saved in settings and applied right away.
#[tauri::command]
fn torrent_set_limits(app: AppHandle, state: tauri::State<'_, AppState>, down_kbps: u32, up_kbps: u32) -> CmdResult<()> {
    state.torrents.set_limits(kbps_limits(down_kbps, up_kbps));
    let mut settings = state.settings.lock().unwrap();
    settings.torrent_down_kbps = Some(down_kbps);
    settings.torrent_up_kbps = Some(up_kbps);
    settings::save(&app, &settings)
}

fn kbps_limits(down_kbps: u32, up_kbps: u32) -> torrent::Limits {
    let bps = |k: u32| (k > 0).then(|| k.saturating_mul(1024));
    torrent::Limits { down_bps: bps(down_kbps), up_bps: bps(up_kbps) }
}

#[tauri::command]
async fn torrent_remove(app: AppHandle, key: String, delete_files: bool) -> CmdResult<()> {
    let torrents = app.state::<AppState>().torrents.clone();
    torrents.remove(&key, delete_files).await
}

#[tauri::command]
async fn pick_cover_image(app: AppHandle) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    Ok(app
        .dialog()
        .file()
        .set_title("Choose a cover picture")
        .add_filter("Pictures", &["jpg", "jpeg", "png", "webp"])
        .blocking_pick_file()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
fn torrent_set_cover(state: tauri::State<'_, AppState>, name: String, path: String) -> CmdResult<()> {
    state.covers.set_custom(&name, Path::new(&path))
}

#[tauri::command]
fn torrent_reset_cover(state: tauri::State<'_, AppState>, name: String) {
    state.covers.reset(&name);
}

#[tauri::command]
async fn torrent_downloading(app: AppHandle) -> CmdResult<bool> {
    let torrents = app.state::<AppState>().torrents.clone();
    Ok(torrents.downloading().await)
}

#[tauri::command]
async fn scrape_web_library(
    app: AppHandle,
    url: String,
    crawl_linked_pages: bool,
) -> CmdResult<web_library::WebLibrary> {
    web_library::scrape(&url, crawl_linked_pages, Some(&app)).await
}

#[tauri::command]
async fn inspect_web_source(url: String) -> CmdResult<web_library::WebSource> {
    web_library::inspect_source(&url).await
}

#[tauri::command]
async fn search_web_source(
    url: String,
    query: String,
) -> CmdResult<web_library::WebLibraryItem> {
    web_library::search_source(&url, &query).await
}

#[tauri::command]
async fn download_web_file(
    app: AppHandle,
    url: String,
    folder: String,
    name: String,
) -> CmdResult<String> {
    web_library::download(&app, &url, Path::new(&folder), &name).await
}

/// Test-only override for the update feed. Never passed on to the installer or the reopened app.
pub const TEST_FEED_VAR: &str = "STEAM_TRANSFER_UPDATE_URL";

/// The updater for GitHub releases. A test feed set through STEAM_TRANSFER_UPDATE_URL is used only
/// when this build accepts it (release builds require https); otherwise it is ignored, so a stray
/// setting can never stop real updates.
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    use tauri_plugin_updater::UpdaterExt;
    if let Some(url) = std::env::var(TEST_FEED_VAR).ok().and_then(|u| u.parse().ok()) {
        if let Ok(test) = app.updater_builder().endpoints(vec![url]).and_then(|b| b.build()) {
            return Ok(test);
        }
    }
    app.updater_builder().build().map_err(|e| e.to_string())
}

#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<Option<UpdateDto>> {
    let update = updater(&app)?.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| UpdateDto { version: u.version.clone(), notes: u.body.clone() }))
}

/// Download the update, then hand over to a helper and close normally.
///
/// We don't use the plugin's `download_and_install`: on Windows it calls `std::process::exit`
/// from a background thread while the window is still open, which can freeze the app halfway
/// through exiting (and a frozen copy then makes the next launch show a black window).
/// Instead a temporary copy of this exe waits for the app to close, runs the installer,
/// waits for it to finish and reopens the app.
#[tauri::command]
async fn install_update(app: AppHandle) -> CmdResult<()> {
    if app.state::<AppState>().queue.busy() {
        return Err("Wait for the transfers in the queue to finish first.".into());
    }
    let update = updater(&app)?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("You already have the latest version.")?;

    let mut downloaded: u64 = 0;
    let progress_app = app.clone();
    // `download` checks the file's signature against the public key built into the app.
    let bytes = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress_app.emit("update-progress", (downloaded, total));
            },
            || {},
        )
        .await
        .map_err(|e| format!("Download failed: {e}"))?;

    let dir = std::env::temp_dir().join("SteamTransfer-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let installer = dir.join(format!("Steam-Transfer_{}_x64-setup.exe", update.version));
    std::fs::write(&installer, &bytes).map_err(|e| format!("Could not save the update: {e}"))?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = dir.join("steam-transfer-update-helper.exe");
    std::fs::copy(&me, &helper).map_err(|e| format!("Could not prepare the updater: {e}"))?;

    let _ = app.emit("update-installing", ());
    std::process::Command::new(&helper)
        .arg("--apply-update")
        .arg(std::process::id().to_string())
        .arg(&installer)
        .arg(&me)
        .spawn()
        .map_err(|e| format!("Could not start the updater: {e}"))?;

    // let the "installing" message show on the display, then close the normal way
    std::thread::sleep(std::time::Duration::from_millis(1200));
    sys::exit_watchdog(std::time::Duration::from_secs(8));
    app.exit(0);
    Ok(())
}

/// Command-line modes that run instead of the app. Returns an exit code when one was handled.
pub fn handle_cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--apply-update") => Some(sys::apply_update(&args[1..])),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // A copy frozen while exiting would block this one from drawing its window.
    sys::close_stuck_copies();
    tauri::Builder::default()
        // Only one copy at a time (two queues copying to the same drive would fight);
        // opening it again just brings the existing window to the front.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol("art", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || {
                let key = request.uri().path().trim_start_matches('/').to_string();
                let path = app.state::<AppState>().art.lock().unwrap().get(&key).cloned();
                let response = match path.and_then(|p| std::fs::read(&p).ok().map(|bytes| (p, bytes))) {
                    Some((path, bytes)) => {
                        let png = path.extension().map(|e| e.eq_ignore_ascii_case("png")).unwrap_or(false);
                        tauri::http::Response::builder()
                            .header("Content-Type", if png { "image/png" } else { "image/jpeg" })
                            .header("Cache-Control", "max-age=600")
                            .body(bytes)
                    }
                    None => tauri::http::Response::builder().status(404).body(Vec::new()),
                };
                responder.respond(response.unwrap());
            });
        })
        .setup(|app| {
            let queue = Queue::new();
            queue.start_worker(app.handle().clone());
            let settings = settings::load(app.handle());
            let data_dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("SteamTransfer"));
            let downloads = app.path().download_dir().unwrap_or_else(|_| data_dir.join("Downloads"));
            let art: covers::ArtMap = Default::default();
            let covers = covers::Covers::new(&data_dir, art.clone());
            let torrents = torrent::Torrents::new(data_dir, downloads, covers.clone());
            torrents.set_limits(kbps_limits(settings.torrent_down_kbps.unwrap_or(0), settings.torrent_up_kbps.unwrap_or(0)));
            // Resume unfinished downloads from last time in the background.
            if torrents.has_saved_session() {
                let t = torrents.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = t.session().await;
                });
            }
            // and ones that were still fetching their info
            let t = torrents.clone();
            tauri::async_runtime::spawn(async move { t.restore_pending() });
            app.manage(AppState { queue, art, settings: Mutex::new(settings), torrents, covers });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_version,
            get_settings,
            save_settings,
            pick_folder,
            scan_library,
            scan_binder,
            find_binders,
            list_libraries,
            create_library,
            detect_library,
            library_state,
            steam_running,
            enqueue,
            cancel_job,
            queue_state,
            open_folder,
            check_update,
            install_update,
            torrent_default_dir,
            torrent_add,
            pick_torrent_file,
            torrent_add_file,
            torrent_list,
            torrent_pause,
            torrent_resume,
            torrent_remove,
            torrent_downloading,
            pick_cover_image,
            torrent_set_cover,
            torrent_reset_cover,
            torrent_set_limits,
            scrape_web_library,
            inspect_web_source,
            search_web_source,
            download_web_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Steam Transfer");
}

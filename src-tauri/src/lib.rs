mod engine;
mod jobs;
mod package;
mod settings;
mod steam;
mod sys;
mod vdf;

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
    art: Mutex<HashMap<String, PathBuf>>,
    settings: Mutex<Settings>,
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

#[tauri::command]
async fn list_libraries() -> CmdResult<Vec<DriveDto>> {
    Ok(steam::libraries().into_iter().map(|l| DriveDto { free_bytes: sys::free_space(&l), path: l }).collect())
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

#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<Option<UpdateDto>> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| UpdateDto { version: u.version.clone(), notes: u.body.clone() }))
}

#[tauri::command]
async fn install_update(app: AppHandle) -> CmdResult<()> {
    use tauri_plugin_updater::UpdaterExt;
    if app.state::<AppState>().queue.busy() {
        return Err("Wait for the transfers in the queue to finish first.".into());
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("You already have the latest version.")?;
    let mut downloaded: u64 = 0;
    let progress_app = app.clone();
    let finished_app = app.clone();
    // On Windows the plugin launches the installer (passive mode) and exits this process;
    // the installer reopens the app when it is done.
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress_app.emit("update-progress", (downloaded, total));
            },
            move || {
                let _ = finished_app.emit("update-installing", ());
                // let the "installing" message show on the display before the app closes
                std::thread::sleep(std::time::Duration::from_millis(1200));
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

// ---------------------------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
            app.manage(AppState { queue, art: Mutex::new(HashMap::new()), settings: Mutex::new(settings::load(app.handle())) });
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
            detect_library,
            library_state,
            steam_running,
            enqueue,
            cancel_job,
            queue_state,
            open_folder,
            check_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Steam Transfer");
}

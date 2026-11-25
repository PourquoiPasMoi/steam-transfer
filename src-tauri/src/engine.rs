//! File synchronisation engine shared by backup, restore and update.
//!
//! * Every file is copied to `<name>.steamtransfer-part` and renamed only when complete,
//!   so an interrupted copy never leaves a truncated file that looks finished.
//! * Files whose size and modified time already match are skipped, which is what makes
//!   both "resume" and "apply update" cheap: only new/changed files are copied.
//! * Optionally mirrors deletions (files removed by a Steam update) into the destination.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub const PART_SUFFIX: &str = ".steamtransfer-part";
const MTIME_TOLERANCE: Duration = Duration::from_secs(3);
const REPORT_EVERY: Duration = Duration::from_millis(120);

#[derive(Debug, Clone)]
pub struct FileJob {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub rel: String,
    pub len: u64,
    pub mtime: Option<SystemTime>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Progress {
    pub stage: String,
    pub file: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub speed: f64,
    pub eta_secs: Option<u64>,
}

#[derive(Debug)]
pub enum Stop {
    Cancelled,
    Failed(String),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Failed(e.to_string())
    }
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Failed(e)
    }
}

pub type StopResult<T> = Result<T, Stop>;

/// What the engine needs from its caller: a cancel flag and somewhere to send progress.
pub struct Ctx<'a> {
    pub cancel: &'a AtomicBool,
    pub progress: &'a (dyn Fn(&Progress) + Sync),
}

impl Ctx<'_> {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn check(&self) -> StopResult<()> {
        if self.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CopyStats {
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub skipped_files: u64,
}

/// Recursively list regular files under `src_root`, pairing each with its location under `dst_root`.
/// Symlinks / junctions are skipped (same as robocopy /XJ) to avoid loops.
pub fn list_tree(src_root: &Path, dst_root: &Path, out: &mut Vec<FileJob>) -> io::Result<()> {
    fn walk(dir: &Path, rel: &str, dst_root: &Path, out: &mut Vec<FileJob>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}\\{name}") };
            if ft.is_dir() {
                walk(&entry.path(), &child_rel, dst_root, out)?;
            } else if ft.is_file() {
                if name.ends_with(PART_SUFFIX) {
                    continue;
                }
                let meta = entry.metadata()?;
                out.push(FileJob {
                    src: entry.path(),
                    dst: dst_root.join(&child_rel),
                    rel: child_rel,
                    len: meta.len(),
                    mtime: meta.modified().ok(),
                });
            }
        }
        Ok(())
    }
    if src_root.is_dir() {
        walk(src_root, "", dst_root, out)?;
    }
    Ok(())
}

fn mtime_close(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let diff = a.duration_since(b).unwrap_or_else(|e| e.duration());
            diff <= MTIME_TOLERANCE
        }
        _ => false,
    }
}

/// Clear the read-only attribute (some games ship read-only files) so we can replace or delete it.
fn make_writable(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
}

/// Delete a file even if it is marked read-only.
pub fn remove_file_force(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(path);
            fs::remove_file(path)
        }
        other => other,
    }
}

/// True when the destination already holds this exact file (same size and modified time).
pub fn is_same(job: &FileJob) -> bool {
    match fs::metadata(&job.dst) {
        Ok(meta) => meta.is_file() && meta.len() == job.len && mtime_close(job.mtime, meta.modified().ok()),
        Err(_) => false,
    }
}

/// Remove leftover `.steamtransfer-part` files from an interrupted run.
pub fn clean_partials(root: &Path) -> usize {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() {
                stack.push(entry.path());
            } else if ft.is_file() && entry.file_name().to_string_lossy().ends_with(PART_SUFFIX) && remove_file_force(&entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Delete files under `dst_root` that are not part of `jobs` (relative paths compared case-insensitively),
/// then remove directories left empty. Returns the relative paths that were deleted.
pub fn mirror_deletions(dst_root: &Path, jobs: &[FileJob]) -> io::Result<Vec<String>> {
    let keep: HashSet<String> = jobs.iter().map(|j| j.rel.to_lowercase()).collect();
    let mut existing = Vec::new();
    list_tree(dst_root, dst_root, &mut existing)?;
    let mut deleted = Vec::new();
    for file in existing {
        if !keep.contains(&file.rel.to_lowercase()) {
            remove_file_force(&file.src)?;
            deleted.push(file.rel);
        }
    }
    if !deleted.is_empty() {
        remove_empty_dirs(dst_root);
    }
    Ok(deleted)
}

fn remove_empty_dirs(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    let mut empty = true;
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
            if remove_empty_dirs(&path) {
                let _ = fs::remove_dir(&path);
            } else {
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    empty
}

/// Bytes that still need copying (for the free-space check).
pub fn bytes_needed(jobs: &[FileJob]) -> u64 {
    jobs.iter().filter(|j| !is_same(j)).map(|j| j.len).sum()
}

struct Meter<'a> {
    ctx: &'a Ctx<'a>,
    stage: String,
    started: Instant,
    last: Instant,
    total_bytes: u64,
    total_files: u64,
    done_bytes: u64,
    done_files: u64,
    copied_bytes: u64,
    remaining_copy: u64,
}

impl Meter<'_> {
    fn emit(&mut self, file: &str, force: bool) {
        if !force && self.last.elapsed() < REPORT_EVERY {
            return;
        }
        self.last = Instant::now();
        let secs = self.started.elapsed().as_secs_f64();
        let speed = if secs > 0.25 { self.copied_bytes as f64 / secs } else { 0.0 };
        let eta = (speed > 1.0).then(|| (self.remaining_copy as f64 / speed) as u64);
        (self.ctx.progress)(&Progress {
            stage: self.stage.clone(),
            file: file.to_string(),
            bytes_done: self.done_bytes,
            bytes_total: self.total_bytes,
            files_done: self.done_files,
            files_total: self.total_files,
            speed,
            eta_secs: eta,
        });
    }
}

/// Copy every job that is not already present. `log` receives one line per action.
pub fn copy_jobs(jobs: &[FileJob], ctx: &Ctx, stage: &str, log: &mut dyn Write) -> StopResult<CopyStats> {
    let mut stats = CopyStats::default();
    let mut meter = Meter {
        ctx,
        stage: stage.to_string(),
        started: Instant::now(),
        last: Instant::now() - REPORT_EVERY,
        total_bytes: jobs.iter().map(|j| j.len).sum(),
        total_files: jobs.len() as u64,
        done_bytes: 0,
        done_files: 0,
        copied_bytes: 0,
        remaining_copy: bytes_needed(jobs),
    };

    for job in jobs {
        ctx.check()?;
        if is_same(job) {
            stats.skipped_files += 1;
            meter.done_bytes += job.len;
            meter.done_files += 1;
            meter.emit(&job.rel, false);
            continue;
        }
        if let Some(parent) = job.dst.parent() {
            fs::create_dir_all(parent).map_err(|e| Stop::Failed(format!("Could not create {}: {e}", parent.display())))?;
        }
        let _ = writeln!(log, "COPY  {}  {}", job.len, job.rel);
        let part = PathBuf::from(format!("{}{}", job.dst.display(), PART_SUFFIX));
        let _ = remove_file_force(&part);

        let base_done = meter.done_bytes;
        let base_copied = meter.copied_bytes;
        let base_remaining = meter.remaining_copy;
        let result = copy_file_with_progress(&job.src, &part, job.len, ctx.cancel, &mut |transferred| {
            meter.done_bytes = base_done + transferred;
            meter.copied_bytes = base_copied + transferred;
            meter.remaining_copy = base_remaining.saturating_sub(transferred);
            meter.emit(&job.rel, false);
        });
        match result {
            Ok(()) => {}
            Err(e) => {
                let _ = remove_file_force(&part);
                if ctx.cancelled() {
                    return Err(Stop::Cancelled);
                }
                return Err(Stop::Failed(format!("Copying {} failed: {e}", job.rel)));
            }
        }
        // CopyFileEx preserves the modified time; set it explicitly too so the fallback path matches.
        make_writable(&part);
        make_writable(&job.dst);
        if let Some(mtime) = job.mtime {
            if let Ok(f) = fs::OpenOptions::new().write(true).open(&part) {
                let _ = f.set_modified(mtime);
            }
        }
        fs::rename(&part, &job.dst).map_err(|e| {
            let _ = remove_file_force(&part);
            Stop::Failed(format!("Could not finalise {}: {e}", job.rel))
        })?;

        stats.copied_files += 1;
        stats.copied_bytes += job.len;
        meter.done_bytes = base_done + job.len;
        meter.copied_bytes = base_copied + job.len;
        meter.remaining_copy = base_remaining.saturating_sub(job.len);
        meter.done_files += 1;
        meter.emit(&job.rel, false);
    }
    meter.emit("", true);
    Ok(stats)
}

/// Quick verification: every destination file exists with the source's exact size.
pub fn verify_sizes(jobs: &[FileJob]) -> Result<(), String> {
    for job in jobs {
        match fs::metadata(&job.dst) {
            Ok(m) if m.len() == job.len => {}
            Ok(m) => return Err(format!("Verification failed: {} is {} bytes, expected {}.", job.rel, m.len(), job.len)),
            Err(_) => return Err(format!("Verification failed: {} is missing.", job.rel)),
        }
    }
    Ok(())
}

/// Deep verification: compare every byte of source and destination. Returns mismatching files.
pub fn deep_compare(jobs: &[FileJob], ctx: &Ctx) -> StopResult<Vec<String>> {
    const BUF: usize = 4 * 1024 * 1024;
    let mut a = vec![0u8; BUF];
    let mut b = vec![0u8; BUF];
    let mut bad = Vec::new();
    let mut meter = Meter {
        ctx,
        stage: "Checking every byte".into(),
        started: Instant::now(),
        last: Instant::now() - REPORT_EVERY,
        total_bytes: jobs.iter().map(|j| j.len).sum(),
        total_files: jobs.len() as u64,
        done_bytes: 0,
        done_files: 0,
        copied_bytes: 0,
        remaining_copy: jobs.iter().map(|j| j.len).sum(),
    };
    for job in jobs {
        ctx.check()?;
        let start = meter.done_bytes;
        let same = (|| -> io::Result<bool> {
            let mut fa = fs::File::open(&job.src)?;
            let mut fb = match fs::File::open(&job.dst) {
                Ok(f) => f,
                Err(_) => return Ok(false),
            };
            if fb.metadata()?.len() != job.len {
                return Ok(false);
            }
            loop {
                if ctx.cancelled() {
                    return Ok(true);
                }
                let n = read_full(&mut fa, &mut a)?;
                let m = read_full(&mut fb, &mut b[..n.max(1)])?;
                if n != m || a[..n] != b[..m] {
                    return Ok(false);
                }
                if n == 0 {
                    return Ok(true);
                }
                meter.done_bytes += n as u64;
                meter.copied_bytes += n as u64;
                meter.remaining_copy = meter.remaining_copy.saturating_sub(n as u64);
                meter.emit(&job.rel, false);
            }
        })()
        .map_err(|e| Stop::Failed(format!("Reading {} failed: {e}", job.rel)))?;
        ctx.check()?;
        if !same {
            bad.push(job.rel.clone());
        }
        meter.done_bytes = start + job.len;
        meter.done_files += 1;
    }
    meter.emit("", true);
    Ok(bad)
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Total file count and bytes under a folder.
pub fn measure(root: &Path) -> (u64, u64) {
    let mut jobs = Vec::new();
    let _ = list_tree(root, root, &mut jobs);
    (jobs.len() as u64, jobs.iter().map(|j| j.len).sum())
}

// ---------------------------------------------------------------------------------------------
// Platform copy

/// Prefix with \\?\ so paths longer than 260 characters work with the Win32 API.
#[cfg(windows)]
fn win_long(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let s = path.as_os_str().to_string_lossy();
    let full = if s.starts_with("\\\\?\\") {
        s.to_string()
    } else if let Some(unc) = s.strip_prefix("\\\\") {
        format!("\\\\?\\UNC\\{unc}")
    } else if path.is_absolute() {
        format!("\\\\?\\{s}")
    } else {
        s.to_string()
    };
    std::ffi::OsStr::new(&full).encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn copy_file_with_progress(src: &Path, dst: &Path, len: u64, cancel: &AtomicBool, on_bytes: &mut dyn FnMut(u64)) -> io::Result<()> {
    use std::ffi::c_void;
    use windows_sys::Win32::Storage::FileSystem::CopyFileExW;
    const COPY_FILE_NO_BUFFERING: u32 = 0x0000_1000;

    struct Data<'a> {
        cancel: &'a AtomicBool,
        on_bytes: &'a mut dyn FnMut(u64),
    }

    unsafe extern "system" fn routine(
        _total: i64,
        transferred: i64,
        _stream_size: i64,
        _stream_transferred: i64,
        _stream: u32,
        _reason: u32,
        _src: windows_sys::Win32::Foundation::HANDLE,
        _dst: windows_sys::Win32::Foundation::HANDLE,
        data: *const c_void,
    ) -> u32 {
        let data = &mut *(data as *mut Data);
        if data.cancel.load(Ordering::Relaxed) {
            return 1; // PROGRESS_CANCEL
        }
        (data.on_bytes)(transferred.max(0) as u64);
        0 // PROGRESS_CONTINUE
    }

    let s = win_long(src);
    let d = win_long(dst);
    let mut data = Data { cancel, on_bytes };
    // Unbuffered I/O is faster and avoids flooding the file cache for big game archives.
    let flags = if len >= 128 * 1024 * 1024 { COPY_FILE_NO_BUFFERING } else { 0 };
    let ok = unsafe {
        CopyFileExW(
            s.as_ptr(),
            d.as_ptr(),
            Some(routine),
            &mut data as *mut Data as *const c_void,
            std::ptr::null_mut(),
            flags,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn copy_file_with_progress(src: &Path, dst: &Path, _len: u64, cancel: &AtomicBool, on_bytes: &mut dyn FnMut(u64)) -> io::Result<()> {
    let mut input = fs::File::open(src)?;
    let mut output = fs::File::create(dst)?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        done += n as u64;
        on_bytes(done);
    }
    output.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("steam-transfer-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn sync(src: &Path, dst: &Path, mirror: bool) -> (CopyStats, Vec<String>) {
        let cancel = AtomicBool::new(false);
        let progress = |_: &Progress| {};
        let ctx = Ctx { cancel: &cancel, progress: &progress };
        let mut jobs = Vec::new();
        list_tree(src, dst, &mut jobs).unwrap();
        clean_partials(dst);
        let deleted = if mirror { mirror_deletions(dst, &jobs).unwrap() } else { vec![] };
        let stats = copy_jobs(&jobs, &ctx, "Copying", &mut io::sink()).ok().unwrap();
        verify_sizes(&jobs).unwrap();
        (stats, deleted)
    }

    #[test]
    fn copies_resumes_and_applies_updates() {
        let root = temp_dir("sync");
        let src = root.join("src");
        let dst = root.join("dst");
        write(&src.join("game.exe"), b"exe-v1");
        write(&src.join("data\\level1.pak"), &vec![7u8; 300_000]);
        write(&src.join("data\\old.pak"), b"to be removed by update");

        // fresh copy
        let (stats, _) = sync(&src, &dst, true);
        assert_eq!(stats.copied_files, 3);
        assert_eq!(fs::read(dst.join("game.exe")).unwrap(), b"exe-v1");

        // resume: a leftover partial file plus nothing changed -> everything skipped, partial removed
        write(&dst.join("data\\level1.pak.steamtransfer-part"), b"half");
        let (stats, _) = sync(&src, &dst, true);
        assert_eq!(stats.copied_files, 0);
        assert_eq!(stats.skipped_files, 3);
        assert!(!dst.join("data\\level1.pak.steamtransfer-part").exists());

        // a game update: one changed file, one new, one deleted
        std::thread::sleep(Duration::from_millis(50));
        write(&src.join("game.exe"), b"exe-v2-bigger");
        write(&src.join("data\\level2.pak"), b"new content");
        fs::remove_file(src.join("data\\old.pak")).unwrap();
        let (stats, deleted) = sync(&src, &dst, true);
        assert_eq!(stats.copied_files, 2);
        assert_eq!(deleted, vec!["data\\old.pak".to_string()]);
        assert_eq!(fs::read(dst.join("game.exe")).unwrap(), b"exe-v2-bigger");
        assert!(!dst.join("data\\old.pak").exists());
        assert_eq!(measure(&src), measure(&dst));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deep_compare_finds_corruption() {
        let root = temp_dir("deep");
        let src = root.join("src");
        let dst = root.join("dst");
        write(&src.join("a.bin"), &vec![1u8; 10_000]);
        write(&src.join("b.bin"), b"hello");
        sync(&src, &dst, false);
        // same size, different content
        write(&dst.join("a.bin"), &vec![2u8; 10_000]);
        let cancel = AtomicBool::new(false);
        let progress = |_: &Progress| {};
        let ctx = Ctx { cancel: &cancel, progress: &progress };
        let mut jobs = Vec::new();
        list_tree(&src, &dst, &mut jobs).unwrap();
        assert_eq!(deep_compare(&jobs, &ctx).ok().unwrap(), vec!["a.bin".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn handles_paths_over_260_chars() {
        let root = temp_dir("long");
        let src = root.join("src");
        let dst = root.join("dst");
        let mut deep = src.clone();
        for i in 0..8 {
            deep = deep.join(format!("very_long_folder_name_for_testing_long_paths_{i:02}"));
        }
        let file = deep.join("asset.bundle");
        assert!(file.as_os_str().len() > 300);
        write(&file, b"deep");
        let (stats, _) = sync(&src, &dst, true);
        assert_eq!(stats.copied_files, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_stops_cleanly() {
        let root = temp_dir("cancel");
        let src = root.join("src");
        let dst = root.join("dst");
        write(&src.join("a.bin"), b"a");
        let cancel = AtomicBool::new(true);
        let progress = |_: &Progress| {};
        let ctx = Ctx { cancel: &cancel, progress: &progress };
        let mut jobs = Vec::new();
        list_tree(&src, &dst, &mut jobs).unwrap();
        assert!(matches!(copy_jobs(&jobs, &ctx, "Copying", &mut io::sink()), Err(Stop::Cancelled)));
        assert!(!dst.join("a.bin").exists());
        let _ = fs::remove_dir_all(&root);
    }
}

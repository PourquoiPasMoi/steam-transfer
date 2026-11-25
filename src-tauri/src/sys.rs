//! Small Windows helpers: free disk space, process detection, drive letters.

use std::path::{Path, PathBuf};

#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // Walk up until we find a directory that exists (destination may not be created yet).
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return None;
        }
    }
    let wide: Vec<u16> = probe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut available: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok != 0 {
        Some(available)
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

/// True when a process with this executable name (case-insensitive) is running.
#[cfg(windows)]
pub fn process_running(exe_name: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if name.eq_ignore_ascii_case(exe_name) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
pub fn process_running(_exe_name: &str) -> bool {
    false
}

pub fn steam_running() -> bool {
    process_running("steam.exe")
}

/// Root folders of all mounted drives (C:\, D:\, ...).
pub fn drive_roots() -> Vec<PathBuf> {
    (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|root| root.exists())
        .collect()
}

#[cfg(windows)]
mod win {
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{CloseHandle, BOOL, FILETIME, HANDLE, HWND, LPARAM, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};

    pub fn processes_named(exe: &str) -> Vec<u32> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut e) != 0 {
                loop {
                    let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(exe) {
                        out.push(e.th32ProcessID);
                    }
                    if Process32NextW(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn has_visible_window(pid: u32) -> bool {
        struct Search {
            pid: u32,
            found: bool,
        }
        unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let s = &mut *(lparam as *mut Search);
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == s.pid && IsWindowVisible(hwnd) != 0 {
                s.found = true;
                return 0;
            }
            1
        }
        let mut s = Search { pid, found: false };
        unsafe { EnumWindows(Some(cb), &mut s as *mut Search as LPARAM) };
        s.found
    }

    fn open(pid: u32, access: u32) -> Option<HANDLE> {
        let h = unsafe { OpenProcess(access, 0, pid) };
        (!h.is_null()).then_some(h)
    }

    pub fn started_at(pid: u32) -> Option<SystemTime> {
        let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return None;
        }
        // FILETIME: 100ns ticks since 1601-01-01
        let ticks = ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64;
        let unix_100ns = ticks.checked_sub(116_444_736_000_000_000)?;
        Some(UNIX_EPOCH + Duration::from_nanos(unix_100ns * 100))
    }

    pub fn terminate(pid: u32) -> bool {
        match open(pid, PROCESS_TERMINATE) {
            Some(h) => {
                let ok = unsafe { TerminateProcess(h, 1) } != 0;
                unsafe { CloseHandle(h) };
                ok
            }
            None => false,
        }
    }

    /// Wait for a process to end; force it closed if it hasn't after `timeout`.
    pub fn wait_or_kill(pid: u32, timeout: Duration) {
        let Some(h) = open(pid, PROCESS_SYNCHRONIZE | PROCESS_TERMINATE) else { return };
        unsafe {
            if WaitForSingleObject(h, timeout.as_millis() as u32) == WAIT_TIMEOUT {
                TerminateProcess(h, 1);
                WaitForSingleObject(h, 5000);
            }
            CloseHandle(h);
        }
    }

    pub fn terminate_self() -> ! {
        unsafe { TerminateProcess(GetCurrentProcess(), 0) };
        std::process::exit(0)
    }

    pub fn file_modified(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }
}

/// Close copies of this app that are stuck without a window (for example one that froze while
/// exiting for an update). A frozen copy can stop a new one from drawing its window.
/// A copy counts as stuck when it has no visible window and either started before this exe file
/// was last replaced, or has been running for more than 15 seconds.
#[cfg(windows)]
pub fn close_stuck_copies() -> usize {
    let Ok(me) = std::env::current_exe() else { return 0 };
    let Some(name) = me.file_name().map(|n| n.to_string_lossy().to_string()) else { return 0 };
    let exe_changed = win::file_modified(&me);
    let my_pid = std::process::id();
    let now = std::time::SystemTime::now();
    let mut closed = 0;
    for pid in win::processes_named(&name) {
        if pid == my_pid || win::has_visible_window(pid) {
            continue;
        }
        let started = win::started_at(pid);
        let older_than_exe = matches!((started, exe_changed), (Some(s), Some(m)) if s < m);
        let long_running = started
            .and_then(|s| now.duration_since(s).ok())
            .map(|age| age > std::time::Duration::from_secs(15))
            .unwrap_or(true);
        if (older_than_exe || long_running) && win::terminate(pid) {
            closed += 1;
        }
    }
    closed
}

#[cfg(not(windows))]
pub fn close_stuck_copies() -> usize {
    0
}

/// Helper mode used by the self-updater (`steam-transfer.exe --apply-update <pid> <installer> <app>`),
/// run from a temporary copy of the exe so the installer can replace the real one:
/// wait for the app to close (force it after 20 s), run the installer, wait for it, reopen the app.
#[cfg(windows)]
pub fn apply_update(args: &[String]) -> i32 {
    let (Some(pid), Some(installer), Some(app)) = (args.first().and_then(|p| p.parse::<u32>().ok()), args.get(1), args.get(2)) else {
        return 2;
    };
    win::wait_or_kill(pid, std::time::Duration::from_secs(20));
    let status = std::process::Command::new(installer).args(["/P", "/UPDATE"]).status();
    let _ = std::process::Command::new(app).spawn();
    match status {
        Ok(s) if s.success() => 0,
        _ => 1,
    }
}

#[cfg(not(windows))]
pub fn apply_update(_args: &[String]) -> i32 {
    2
}

/// Last resort if a normal exit hangs: end this process after `after`.
pub fn exit_watchdog(after: std::time::Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        #[cfg(windows)]
        win::terminate_self();
        #[cfg(not(windows))]
        std::process::exit(0);
    });
}

/// Open a folder in Explorer.
pub fn open_in_explorer(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("explorer.exe").arg(path).spawn().map(|_| ())
}

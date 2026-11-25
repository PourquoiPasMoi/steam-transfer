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

/// Open a folder in Explorer.
pub fn open_in_explorer(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("explorer.exe").arg(path).spawn().map(|_| ())
}

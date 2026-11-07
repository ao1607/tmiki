//! A full disk, in tmi's words (issue #42): tmi/diskspace.py. The recording of
//! the files a failed stage wrote is `tmi_store::store::track_*`.

use std::path::Path;

/// `TMI_TEST_FREE_BYTES` stands in for the real figure in the tests.
pub fn free_bytes(location: &Path) -> u64 {
    if let Some(forced) = std::env::var("TMI_TEST_FREE_BYTES").ok().filter(|v| !v.is_empty()).and_then(|v| v.parse().ok()) {
        return forced;
    }
    let mut probe = location;
    while !probe.exists() {
        match probe.parent() {
            Some(p) if p != probe => probe = p,
            _ => break,
        }
    }
    platform_free(probe).unwrap_or(u64::MAX)
}

#[cfg(windows)]
fn platform_free(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free_to_caller: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and the out pointers are valid or null.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_to_caller, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free_to_caller)
}

#[cfg(unix)]
fn platform_free(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is NUL-terminated and `st` is a valid out buffer.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (rc == 0).then(|| (st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// `human_size(size)`.
pub fn human_size(size: u64) -> String {
    if size >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", size as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if size >= 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{} KB", size / 1024)
    } else {
        format!("{size} bytes")
    }
}

/// `not_enough_message(rel, needed, free)`.
pub fn not_enough_message(rel: &str, needed: u64, free: u64) -> String {
    format!(
        "not enough disk space to record {rel}: it needs at least {} and {} is free. Free some space and run the command again",
        human_size(needed),
        human_size(free)
    )
}

/// `disk_full_message(rel, needed, free)`.
pub fn disk_full_message(rel: &str, needed: u64, free: u64) -> String {
    format!(
        "the disk is full while recording {rel} (at least {} needed, {} free); tmi removed what it had written for it. \
         Free some space and run the command again",
        human_size(needed),
        human_size(free)
    )
}

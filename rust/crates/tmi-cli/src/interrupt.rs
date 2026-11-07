//! Ctrl+C (and SIGTERM on Unix) must not leave `index.lock` behind (issue
//! #35): the handler removes the locks this process holds, then the signal
//! takes its default course. Python's `finally` / `atexit` do the same for
//! `python -m tmi`.

#[cfg(windows)]
pub fn install() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        tmi_repo::lockfile::release_all();
        0 // not handled: the process ends as it would have
    }
    // SAFETY: a plain handler routine that only removes files.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(unix)]
pub fn install() {
    extern "C" fn handler(sig: libc::c_int) {
        tmi_repo::lockfile::release_all();
        // SAFETY: back to the default action, then the signal again.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    // SAFETY: `handler` is an `extern "C" fn(c_int)`.
    unsafe {
        libc::signal(libc::SIGINT, handler as usize);
        libc::signal(libc::SIGTERM, handler as usize);
    }
}

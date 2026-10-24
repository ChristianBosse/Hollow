//! Broker-side desktop hardening: keeping windows out of screen capture
//! (WIN-01) and running a single broker at a time.

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

use crate::wide::{last_error, wide};

/// Hides a window from screenshots, screen recording, screen sharing and
/// Windows Recall: captures show it as black or leave it out.
pub fn exclude_window_from_capture(hwnd: isize) -> io::Result<()> {
    // SAFETY: SetWindowDisplayAffinity validates the handle itself.
    if unsafe { SetWindowDisplayAffinity(hwnd as _, WDA_EXCLUDEFROMCAPTURE) } == 0 {
        return Err(last_error("SetWindowDisplayAffinity"));
    }
    Ok(())
}

/// Closes the console window Windows opened just for us (a double-clicked
/// `hollow.exe`), but leaves a terminal the user started us from alone.
/// Later writes to stdout/stderr are silently dropped by std.
pub fn detach_own_console() {
    let mut pids = [0u32; 2];
    // SAFETY: the buffer holds two entries; the call reports how many exist.
    let attached = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if attached == 1 {
        // SAFETY: detaching from our own console has no preconditions.
        unsafe { FreeConsole() };
    }
}

/// Held for the broker's lifetime. Cleanup at exit removes the sandbox
/// profiles every child depends on, so two brokers must never overlap.
#[derive(Debug)]
pub struct InstanceLock(#[allow(dead_code)] OwnedHandle);

/// Returns `None` if another Hollow broker is already running in this session.
pub fn single_instance() -> io::Result<Option<InstanceLock>> {
    let name = wide("Local\\Hollow.Broker");
    // SAFETY: NUL-terminated name, default security.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(last_error("CreateMutexW"));
    }
    // SAFETY: called right after CreateMutexW on this thread.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // SAFETY: fresh handle we own.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    Ok(if existed { None } else { Some(InstanceLock(handle)) })
}

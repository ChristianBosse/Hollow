use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};

/// NUL-terminated UTF-16 for the Windows API.
pub(crate) fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Copies a NUL-terminated UTF-16 string.
///
/// # Safety
/// `p` must point to a valid NUL-terminated UTF-16 string.
pub(crate) unsafe fn from_wide_ptr(p: *const u16) -> OsString {
    let mut len = 0;
    // SAFETY: the caller guarantees `p` is NUL-terminated.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` elements before the terminator are readable.
    OsString::from_wide(unsafe { std::slice::from_raw_parts(p, len) })
}

pub(crate) fn last_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what} failed: {e}"))
}

pub(crate) fn win32_error(what: &str, code: u32) -> io::Error {
    let e = io::Error::from_raw_os_error(code as i32);
    io::Error::new(e.kind(), format!("{what} failed: {e}"))
}

pub(crate) fn hresult_error(what: &str, hr: i32) -> io::Error {
    io::Error::other(format!("{what} failed: HRESULT {:#010x}", hr as u32))
}

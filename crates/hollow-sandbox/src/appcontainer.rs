use std::io;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::FreeSid;
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};

use crate::wide::{from_wide_ptr, hresult_error, last_error, wide};

/// HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)
const HRESULT_ALREADY_EXISTS: i32 = 0x800700B7_u32 as i32;

/// HRESULT_FROM_WIN32(ERROR_NOT_FOUND / ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND)
const HRESULT_NOT_FOUND: [i32; 3] = [0x80070490_u32 as i32, 0x80070002_u32 as i32, 0x80070003_u32 as i32];

/// An AppContainer identity. Its SID is derived from the name, so it is the
/// same on every run.
///
/// Windows only starts AppContainer processes whose profile is registered
/// (otherwise CreateProcessW fails with ERROR_FILE_NOT_FOUND), and
/// registering one writes registry entries and an `%LOCALAPPDATA%\Packages`
/// folder. So the broker registers profiles for the session only and calls
/// [`AppContainer::delete_profile`] on exit, and again at the next start in
/// case it crashed.
#[derive(Debug)]
pub struct AppContainer {
    name: String,
    sid: *mut core::ffi::c_void,
}

impl AppContainer {
    /// Registers the profile if needed and returns its identity.
    pub fn register(name: &str) -> io::Result<Self> {
        let wname = wide(name);
        let mut sid = null_mut();
        // SAFETY: all strings are NUL-terminated; `sid` receives an owned SID.
        let hr = unsafe {
            CreateAppContainerProfile(wname.as_ptr(), wname.as_ptr(), wname.as_ptr(), std::ptr::null(), 0, &mut sid)
        };
        if hr == HRESULT_ALREADY_EXISTS {
            return Self::derive(name);
        }
        if hr < 0 {
            return Err(hresult_error("registering AppContainer profile", hr));
        }
        Ok(Self { name: name.to_owned(), sid })
    }

    /// The identity alone, without registering anything.
    pub fn derive(name: &str) -> io::Result<Self> {
        let wname = wide(name);
        let mut sid = null_mut();
        // SAFETY: `wname` is NUL-terminated; `sid` receives an owned SID.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(wname.as_ptr(), &mut sid) };
        if hr < 0 {
            return Err(hresult_error("deriving AppContainer SID", hr));
        }
        Ok(Self { name: name.to_owned(), sid })
    }

    /// Unregisters the profile and deletes its folder. Returns whether one
    /// existed.
    pub fn delete_profile(name: &str) -> io::Result<bool> {
        let wname = wide(name);
        // SAFETY: `wname` is NUL-terminated.
        let hr = unsafe { DeleteAppContainerProfile(wname.as_ptr()) };
        if hr >= 0 {
            Ok(true)
        } else if HRESULT_NOT_FOUND.contains(&hr) {
            Ok(false)
        } else {
            Err(hresult_error("deleting AppContainer profile", hr))
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn sid(&self) -> *mut core::ffi::c_void {
        self.sid
    }

    pub fn sid_string(&self) -> io::Result<String> {
        let mut s = null_mut();
        // SAFETY: `self.sid` is a valid SID; `s` receives a LocalAlloc'd string.
        if unsafe { ConvertSidToStringSidW(self.sid, &mut s) } == 0 {
            return Err(last_error("ConvertSidToStringSidW"));
        }
        // SAFETY: `s` is NUL-terminated and owned by us until LocalFree.
        let out = unsafe { from_wide_ptr(s) };
        unsafe { LocalFree(s.cast()) };
        Ok(out.to_string_lossy().into_owned())
    }
}

impl Drop for AppContainer {
    fn drop(&mut self) {
        // SAFETY: the SID came from DeriveAppContainerSidFromAppContainerName,
        // which must be freed with FreeSid.
        unsafe { FreeSid(self.sid) };
    }
}

/// Capabilities a container may be granted. The renderer gets none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Outbound internet connections. Only the network process has this.
    InternetClient,
}

impl Capability {
    fn sid_string(self) -> &'static str {
        match self {
            Capability::InternetClient => "S-1-15-3-1",
        }
    }
}

/// A SID allocated by `ConvertStringSidToSidW`.
pub(crate) struct OwnedSid(pub(crate) *mut core::ffi::c_void);

impl Drop for OwnedSid {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSidToSidW, freed with LocalFree.
        unsafe { LocalFree(self.0.cast()) };
    }
}

pub(crate) fn capability_sid(cap: Capability) -> io::Result<OwnedSid> {
    let s = wide(cap.sid_string());
    let mut sid = null_mut();
    // SAFETY: `s` is NUL-terminated; `sid` receives a LocalAlloc'd SID.
    if unsafe { ConvertStringSidToSidW(s.as_ptr(), &mut sid) } == 0 {
        return Err(last_error("ConvertStringSidToSidW"));
    }
    Ok(OwnedSid(sid))
}

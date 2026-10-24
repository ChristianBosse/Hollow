use std::io;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ACCESS_MODE, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};

use crate::wide::{wide, win32_error};

/// Lets `sid` read and execute `path` (and nothing else). AppContainers can't
/// open files in the user's folders by default, including their own `.exe`.
///
/// In an installed build this ACE comes from the installer, on an install
/// directory nothing else lives in. Portable and dev builds grant it at
/// launch and [`revoke`] it when the broker exits.
pub(crate) fn grant_read_execute(path: &Path, sid: *mut core::ffi::c_void) -> io::Result<()> {
    set_entry(path, sid, GRANT_ACCESS)
}

/// Removes every ACE for `sid` from `path`.
pub(crate) fn revoke(path: &Path, sid: *mut core::ffi::c_void) -> io::Result<()> {
    set_entry(path, sid, REVOKE_ACCESS)
}

fn set_entry(path: &Path, sid: *mut core::ffi::c_void, mode: ACCESS_MODE) -> io::Result<()> {
    let wpath = wide(path);
    let mut old_dacl: *mut ACL = null_mut();
    let mut sd = null_mut();
    // SAFETY: `wpath` is NUL-terminated; out-pointers are valid. `sd` owns
    // the returned descriptor, which `old_dacl` points into.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(win32_error(&format!("reading ACL of {}", path.display()), err));
    }

    let access = EXPLICIT_ACCESS_W {
        grfAccessPermissions: if mode == GRANT_ACCESS { FILE_GENERIC_READ | FILE_GENERIC_EXECUTE } else { 0 },
        grfAccessMode: mode,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
            ptstrName: sid.cast(),
        },
    };
    let mut new_dacl: *mut ACL = null_mut();
    // SAFETY: one valid entry; `old_dacl` stays alive until `sd` is freed.
    let err = unsafe { SetEntriesInAclW(1, &access, old_dacl, &mut new_dacl) };
    let result = if err != 0 {
        Err(win32_error("SetEntriesInAclW", err))
    } else {
        // SAFETY: `new_dacl` is a valid ACL from SetEntriesInAclW.
        let err = unsafe {
            SetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                new_dacl,
                std::ptr::null(),
            )
        };
        unsafe { LocalFree(new_dacl.cast()) };
        if err != 0 { Err(win32_error(&format!("writing ACL of {}", path.display()), err)) } else { Ok(()) }
    };
    // SAFETY: `sd` was allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(sd) };
    result
}

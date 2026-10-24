use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS,
    JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
    JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
};

use crate::wide::last_error;

/// A job that holds exactly one process, caps its memory, kills it when the
/// broker lets go, and blocks clipboard, desktop and global-atom access.
pub(crate) fn create_job(max_memory: usize) -> io::Result<OwnedHandle> {
    // SAFETY: no name, default security.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(last_error("CreateJobObjectW"));
    }
    // SAFETY: `raw` is a fresh handle we own.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };

    // SAFETY: plain-data struct; all-zero is a valid starting value.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit = max_memory;
    set(&job, JobObjectExtendedLimitInformation, &limits)?;

    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };
    set(&job, JobObjectBasicUIRestrictions, &ui)?;
    Ok(job)
}

fn set<T>(job: &OwnedHandle, class: i32, info: &T) -> io::Result<()> {
    // SAFETY: `info` is the struct matching `class`, passed with its size.
    let ok = unsafe {
        SetInformationJobObject(job.as_raw_handle(), class, (info as *const T).cast(), std::mem::size_of::<T>() as u32)
    };
    if ok == 0 { Err(last_error("SetInformationJobObject")) } else { Ok(()) }
}

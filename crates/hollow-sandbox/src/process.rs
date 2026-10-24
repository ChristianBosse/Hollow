use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;

use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
    SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, OpenProcessToken, PROCESS_MITIGATION_POLICY,
    ProcessDynamicCodePolicy, ProcessSignaturePolicy, ProcessSystemCallDisablePolicy, SetProcessMitigationPolicy,
    TerminateProcess,
};

use crate::wide::last_error;

/// Must be the first thing a sandboxed child does. Fails closed: if the
/// process is not inside an AppContainer it refuses to continue.
pub fn enter_sandbox() -> io::Result<()> {
    harden_crash_handling();
    if !is_app_container()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not running inside an AppContainer; refusing to start (launch Hollow through hollow.exe)",
        ));
    }
    // CIG: from now on only Microsoft-signed DLLs may load. Applied here
    // rather than at creation so our own (unsigned dev) image can start.
    set_policy(ProcessSignaturePolicy, 1)?;
    Ok(())
}

/// Keeps crashes away from Windows Error Reporting and its dumps (WIN-03):
/// no error dialogs, and unhandled exceptions end the process immediately.
///
/// Fail-fast exceptions bypass the filter; those are covered by the job's
/// die-on-unhandled-exception limit for children and by Phase 3 for the broker.
pub fn harden_crash_handling() {
    // SAFETY: plain flag setters; the filter only calls TerminateProcess.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        SetUnhandledExceptionFilter(Some(terminate_on_exception));
    }
}

/// Exits with the exception code itself, so the broker can report what
/// happened without any dump being written.
unsafe extern "system" fn terminate_on_exception(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the OS passes valid exception pointers to the filter.
    let record = unsafe { info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) };
    let code = record.map_or(1, |r| r.ExceptionCode as u32);
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "fatal exception {code:#010x}; terminating without a crash dump");
    if let Some(r) = record
        && matches!(code, DELAYLOAD_MOD_NOT_FOUND | DELAYLOAD_PROC_NOT_FOUND)
    {
        // SAFETY: for these codes the first parameter is the loader's DelayLoadInfo.
        let _ =
            writeln!(stderr, "{}", unsafe { describe_delay_load(r.ExceptionInformation[0] as *const DelayLoadInfo) });
        let _ = writeln!(stderr, "{}", std::backtrace::Backtrace::force_capture());
    }
    // SAFETY: terminating the current process is always valid.
    unsafe { TerminateProcess(GetCurrentProcess(), code) };
    0
}

/// VcppException(ERROR_SEVERITY_ERROR, ERROR_MOD_NOT_FOUND / ERROR_PROC_NOT_FOUND)
const DELAYLOAD_MOD_NOT_FOUND: u32 = 0xC06D_007E;
const DELAYLOAD_PROC_NOT_FOUND: u32 = 0xC06D_007F;

/// `DelayLoadInfo` from delayimp.h (x64 layout).
#[repr(C)]
struct DelayLoadInfo {
    cb: u32,
    pidd: *const core::ffi::c_void,
    ppfn: *mut core::ffi::c_void,
    sz_dll: *const core::ffi::c_char,
    import_by_name: i32,
    /// Procedure name, or the ordinal when `import_by_name` is 0.
    proc_name_or_ordinal: *const core::ffi::c_char,
    hmod_cur: *mut core::ffi::c_void,
    pfn_cur: *mut core::ffi::c_void,
    last_error: u32,
}

/// Names the delay-loaded DLL and function that failed to load.
///
/// # Safety
/// `info` must be null or point to the loader's DelayLoadInfo.
unsafe fn describe_delay_load(info: *const DelayLoadInfo) -> String {
    // SAFETY: per the caller; the loader keeps the strings alive.
    unsafe {
        let Some(info) = info.as_ref() else { return "delay-load failure (no details)".into() };
        let dll =
            if info.sz_dll.is_null() { "?".into() } else { std::ffi::CStr::from_ptr(info.sz_dll).to_string_lossy() };
        let func = if info.import_by_name != 0 && !info.proc_name_or_ordinal.is_null() {
            std::ffi::CStr::from_ptr(info.proc_name_or_ordinal).to_string_lossy().into_owned()
        } else {
            format!("#{}", info.proc_name_or_ordinal as usize)
        };
        format!("delay-loaded {dll}!{func} failed to load (Win32 error {})", info.last_error)
    }
}

pub fn is_app_container() -> io::Result<bool> {
    let mut token = null_mut();
    // SAFETY: pseudo-handle for the current process; valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    // SAFETY: fresh handle we own.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut value: u32 = 0;
    let mut len = 0;
    // SAFETY: TokenIsAppContainer returns a DWORD.
    let ok = unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenIsAppContainer, (&mut value as *mut u32).cast(), 4, &mut len)
    };
    if ok == 0 {
        return Err(last_error("GetTokenInformation"));
    }
    Ok(value != 0)
}

/// The sandbox this process is actually running in, read back from Windows.
pub fn sandbox_report() -> hollow_ipc::SandboxReport {
    let m = active_mitigations().ok();
    let probe = vec![0u8; 64 * 1024];
    hollow_ipc::SandboxReport {
        app_container: is_app_container().unwrap_or(false),
        dynamic_code_prohibited: m.is_some_and(|m| m.dynamic_code_prohibited),
        win32k_disabled: m.is_some_and(|m| m.win32k_disabled),
        microsoft_signed_only: m.is_some_and(|m| m.microsoft_signed_only),
        memory_locked: crate::is_page_locked(probe.as_ptr()).unwrap_or(false),
    }
}

/// Mitigations actually in force in this process, for the self-test.
#[derive(Debug, Clone, Copy)]
pub struct ActiveMitigations {
    pub dynamic_code_prohibited: bool,
    pub win32k_disabled: bool,
    pub microsoft_signed_only: bool,
}

pub fn active_mitigations() -> io::Result<ActiveMitigations> {
    Ok(ActiveMitigations {
        dynamic_code_prohibited: get_policy(ProcessDynamicCodePolicy)? & 1 != 0,
        win32k_disabled: get_policy(ProcessSystemCallDisablePolicy)? & 1 != 0,
        microsoft_signed_only: get_policy(ProcessSignaturePolicy)? & 1 != 0,
    })
}

// All three policies used here are a single DWORD of flags, bit 0 being the
// one we care about.
fn get_policy(policy: PROCESS_MITIGATION_POLICY) -> io::Result<u32> {
    let mut flags: u32 = 0;
    // SAFETY: each policy struct is one DWORD.
    let ok = unsafe { GetProcessMitigationPolicy(GetCurrentProcess(), policy, (&mut flags as *mut u32).cast(), 4) };
    if ok == 0 { Err(last_error("GetProcessMitigationPolicy")) } else { Ok(flags) }
}

fn set_policy(policy: PROCESS_MITIGATION_POLICY, flags: u32) -> io::Result<()> {
    // SAFETY: each policy struct is one DWORD.
    let ok = unsafe { SetProcessMitigationPolicy(policy, (&flags as *const u32).cast(), 4) };
    if ok == 0 { Err(last_error("SetProcessMitigationPolicy")) } else { Ok(()) }
}

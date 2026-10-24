use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::{SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES};
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::acl::grant_read_execute;
use crate::appcontainer::{AppContainer, Capability, OwnedSid, capability_sid};
use crate::job::create_job;
use crate::wide::{last_error, wide};

// ProcThreadAttributeValue(number, thread = false, input = true, additive = false)
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
const PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY: usize = 0x0002_0007;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x0002_0009;
const PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY: usize = 0x0002_000E;
const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;

const PROCESS_CREATION_CHILD_PROCESS_RESTRICTED: u32 = 0x01;
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 0x01;
const SE_GROUP_ENABLED: u32 = 0x04;

// PROCESS_CREATION_MITIGATION_POLICY_*_ALWAYS_ON (winbase.h)
const FORCE_RELOCATE_IMAGES: u64 = 1 << 8;
const HEAP_TERMINATE: u64 = 1 << 12;
const BOTTOM_UP_ASLR: u64 = 1 << 16;
const HIGH_ENTROPY_ASLR: u64 = 1 << 20;
const STRICT_HANDLE_CHECKS: u64 = 1 << 24;
const WIN32K_SYSTEM_CALL_DISABLE: u64 = 1 << 28;
const EXTENSION_POINT_DISABLE: u64 = 1 << 32;
const PROHIBIT_DYNAMIC_CODE: u64 = 1 << 36;
const FONT_DISABLE: u64 = 1 << 48;
const IMAGE_LOAD_NO_REMOTE: u64 = 1 << 52;
const IMAGE_LOAD_NO_LOW_LABEL: u64 = 1 << 56;
const IMAGE_LOAD_PREFER_SYSTEM32: u64 = 1 << 60;

/// Process-creation mitigation policy (architecture §3). Signature policy
/// (CIG) is applied by the child itself in [`crate::enter_sandbox`], once its
/// own image is loaded.
#[derive(Debug, Clone, Copy)]
pub struct Mitigations {
    flags: u64,
}

impl Mitigations {
    const BASE: u64 = FORCE_RELOCATE_IMAGES
        | HEAP_TERMINATE
        | BOTTOM_UP_ASLR
        | HIGH_ENTROPY_ASLR
        | STRICT_HANDLE_CHECKS
        | EXTENSION_POINT_DISABLE
        | PROHIBIT_DYNAMIC_CODE
        | IMAGE_LOAD_NO_REMOTE
        | IMAGE_LOAD_NO_LOW_LABEL
        | IMAGE_LOAD_PREFER_SYSTEM32;

    /// Renderers: everything, including Win32k lockdown and no non-system fonts.
    pub fn renderer() -> Self {
        Self { flags: Self::BASE | WIN32K_SYSTEM_CALL_DISABLE | FONT_DISABLE }
    }

    /// Network process: the same, including Win32k lockdown.
    pub fn network() -> Self {
        Self { flags: Self::BASE | WIN32K_SYSTEM_CALL_DISABLE | FONT_DISABLE }
    }
}

#[derive(Debug)]
pub struct SandboxConfig<'a> {
    pub container: &'a AppContainer,
    pub exe: &'a Path,
    pub args: Vec<OsString>,
    pub capabilities: &'a [Capability],
    /// Less Privileged AppContainer: also drops the ALL APPLICATION PACKAGES
    /// group, so files and services shared with ordinary apps are off-limits.
    pub less_privileged: bool,
    pub mitigations: Mitigations,
    pub max_memory: usize,
    /// Added to the minimal environment every child gets.
    pub extra_env: Vec<(String, OsString)>,
}

/// A child running in its AppContainer, inside a kill-on-close job. Dropping
/// it kills the child.
#[derive(Debug)]
pub struct SandboxedProcess {
    process: OwnedHandle,
    _job: OwnedHandle,
    pid: u32,
    /// Broker → child (the child's stdin).
    pub stdin: Option<File>,
    /// Child → broker (the child's stdout).
    pub stdout: Option<File>,
    /// The child's stderr, for logs.
    pub stderr: Option<File>,
}

impl SandboxedProcess {
    pub fn spawn(cfg: &SandboxConfig<'_>) -> io::Result<Self> {
        let exe = cfg.exe.canonicalize()?;
        let dir = exe.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let exe = strip_verbatim(&exe);
        let dir = strip_verbatim(&dir);

        // The container must be able to load its own image.
        grant_read_execute(&dir, cfg.container.sid())?;
        grant_read_execute(&exe, cfg.container.sid())?;

        let (child_stdin, parent_stdin) = pipe(Side::Read)?;
        let (parent_stdout, child_stdout) = pipe(Side::Write)?;
        let (parent_stderr, child_stderr) = pipe(Side::Write)?;

        let cap_sids: Vec<OwnedSid> = cfg.capabilities.iter().map(|c| capability_sid(*c)).collect::<io::Result<_>>()?;
        let mut cap_attrs: Vec<SID_AND_ATTRIBUTES> =
            cap_sids.iter().map(|s| SID_AND_ATTRIBUTES { Sid: s.0, Attributes: SE_GROUP_ENABLED }).collect();
        let security = SECURITY_CAPABILITIES {
            AppContainerSid: cfg.container.sid(),
            Capabilities: if cap_attrs.is_empty() { null_mut() } else { cap_attrs.as_mut_ptr() },
            CapabilityCount: cap_attrs.len() as u32,
            Reserved: 0,
        };
        let mitigation: [u64; 2] = [cfg.mitigations.flags, 0];
        let child_policy = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
        let lpac_policy = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
        let inherit: [HANDLE; 3] =
            [child_stdin.as_raw_handle(), child_stdout.as_raw_handle(), child_stderr.as_raw_handle()];

        let mut attrs = AttributeList::new(if cfg.less_privileged { 5 } else { 4 })?;
        attrs.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
        attrs.set(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigation)?;
        attrs.set(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, &child_policy)?;
        attrs.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherit)?;
        if cfg.less_privileged {
            attrs.set(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY, &lpac_policy)?;
        }

        // SAFETY: plain-data struct; all-zero is valid.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = inherit[0];
        si.StartupInfo.hStdOutput = inherit[1];
        si.StartupInfo.hStdError = inherit[2];
        si.lpAttributeList = attrs.as_ptr();

        let wexe = wide(&exe);
        let wdir = wide(&dir);
        let mut cmdline = command_line(&exe, &cfg.args);
        let env = environment_block(&cfg.extra_env);
        // SAFETY: zeroed out-struct.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer refers to a live, correctly-typed value; the
        // attribute list and everything it points at outlive this call.
        let ok = unsafe {
            CreateProcessW(
                wexe.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                wdir.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(last_error(&format!("starting {} in AppContainer {}", exe.display(), cfg.container.name())));
        }
        // SAFETY: CreateProcessW returned fresh handles that we now own.
        let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
        let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };

        // The child must be in its job before it runs a single instruction.
        let job = match create_job(cfg.max_memory).and_then(|job| {
            // SAFETY: both handles are valid.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
                Err(last_error("AssignProcessToJobObject"))
            } else {
                Ok(job)
            }
        }) {
            Ok(job) => job,
            Err(e) => {
                // SAFETY: valid process handle.
                unsafe { TerminateProcess(process.as_raw_handle(), 1) };
                return Err(e);
            }
        };
        // SAFETY: valid, suspended thread.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(last_error("ResumeThread"));
        }

        // Our copies of the child's pipe ends close here, so EOF propagates.
        drop((child_stdin, child_stdout, child_stderr));
        Ok(Self {
            process,
            _job: job,
            pid: pi.dwProcessId,
            stdin: Some(File::from(parent_stdin)),
            stdout: Some(File::from(parent_stdout)),
            stderr: Some(File::from(parent_stderr)),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Waits up to `timeout`. Returns the exit code if the child has exited.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<u32>> {
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        // SAFETY: valid process handle.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), ms) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: valid handle and out-pointer.
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(last_error("GetExitCodeProcess"));
                }
                Ok(Some(code))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(last_error("WaitForSingleObject")),
        }
    }

    pub fn kill(&self) {
        // SAFETY: valid process handle; failure means it already exited.
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
    }
}

/// Undoes the file access [`SandboxedProcess::spawn`] granted `container` on
/// `exe` and its folder. The broker calls this when it exits, and at startup
/// to clean up after a crash.
pub fn revoke_image_access(container: &AppContainer, exe: &Path) -> io::Result<()> {
    let exe = exe.canonicalize()?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    crate::acl::revoke(&strip_verbatim(&exe), container.sid())?;
    crate::acl::revoke(&strip_verbatim(&dir), container.sid())
}

enum Side {
    /// The child reads from this pipe.
    Read,
    /// The child writes to this pipe.
    Write,
}

/// Returns `(read end, write end)`. Only the child's end is inheritable.
fn pipe(child: Side) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w): (RawHandle, RawHandle) = (null_mut(), null_mut());
    // SAFETY: out-pointers are valid; default (non-inheritable) security.
    if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) } == 0 {
        return Err(last_error("CreatePipe"));
    }
    // SAFETY: both handles are fresh and ours.
    let (r, w) = unsafe { (OwnedHandle::from_raw_handle(r), OwnedHandle::from_raw_handle(w)) };
    let child_end = match child {
        Side::Read => &r,
        Side::Write => &w,
    };
    // SAFETY: valid handle.
    if unsafe { SetHandleInformation(child_end.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(last_error("SetHandleInformation"));
    }
    Ok((r, w))
}

struct AttributeList {
    buf: Vec<usize>,
}

impl AttributeList {
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: size query; fails with ERROR_INSUFFICIENT_BUFFER by design.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        let mut buf = vec![0usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `buf` is at least `size` bytes and pointer-aligned.
        if unsafe { InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &mut size) } == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        Ok(Self { buf })
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr().cast()
    }

    /// The list stores a pointer to `value`, so it must outlive process creation.
    fn set<T>(&mut self, attribute: usize, value: &T) -> io::Result<()> {
        // SAFETY: `value` is valid for `size_of::<T>()` bytes and outlives
        // the list's use in CreateProcessW (enforced by the caller's scope).
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                attribute,
                (value as *const T).cast(),
                size_of::<T>(),
                null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 { Err(last_error("UpdateProcThreadAttribute")) } else { Ok(()) }
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// `\\?\C:\...` → `C:\...`; CreateProcessW and the loader prefer plain paths.
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Children get a minimal environment: no user name, PATH or other variables
/// from the broker. `LOCALAPPDATA` is required: without it CreateProcessW
/// can't set up the AppContainer and fails with ERROR_ENVVAR_NOT_FOUND.
fn environment_block(extra: &[(String, OsString)]) -> Vec<u16> {
    let mut vars: Vec<(String, OsString)> = ["SystemRoot", "SystemDrive", "LOCALAPPDATA"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (k.to_string(), v)))
        .chain(extra.iter().cloned())
        .collect();
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        let mut entry = OsString::from(k);
        entry.push("=");
        entry.push(v);
        block.extend(wide(entry));
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    block
}

fn command_line(exe: &Path, args: &[OsString]) -> Vec<u16> {
    let mut line = String::new();
    quote_arg(&exe.to_string_lossy(), &mut line);
    for a in args {
        line.push(' ');
        quote_arg(&a.to_string_lossy(), &mut line);
    }
    wide(line)
}

/// Quotes one argument the way `CommandLineToArgvW` and the MSVC CRT parse it.
fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> String {
        let mut out = String::new();
        quote_arg(s, &mut out);
        out
    }

    #[test]
    fn quoting() {
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(q(r#"a"b"#), r#""a\"b""#);
        assert_eq!(q(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }

    #[test]
    fn environment_is_minimal() {
        let block = String::from_utf16_lossy(&environment_block(&[]));
        assert!(!block.to_ascii_uppercase().contains("USERPROFILE="));
        assert!(!block.to_ascii_uppercase().contains("PATH="));
        assert!(block.ends_with("\0\0"));
    }
}

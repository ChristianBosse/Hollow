//! Windows sandboxing for Hollow's child processes (architecture §2–§3).
//!
//! The broker uses [`SandboxedProcess::spawn`] to start the renderer and the
//! network process inside AppContainers with exploit mitigations and a job
//! object. Each child calls [`enter_sandbox`] first thing in `main`, so a
//! child that somehow starts outside its sandbox refuses to run.
//!
//! This is the only Hollow crate allowed to use `unsafe`.

#[cfg(not(windows))]
compile_error!("Hollow currently supports Windows only.");

mod acl;
mod appcontainer;
mod desktop;
mod job;
mod launch;
mod memory;
mod process;
mod wide;

pub use memory::{LockStats, LockedAlloc, is_page_locked, lock_stats, reserve_lockable_memory};

pub use appcontainer::{AppContainer, Capability};
pub use desktop::{InstanceLock, detach_own_console, exclude_window_from_capture, single_instance};
pub use launch::{Mitigations, SandboxConfig, SandboxedProcess, revoke_image_access};
pub use process::{
    ActiveMitigations, active_mitigations, enter_sandbox, harden_crash_handling, is_app_container, sandbox_report,
};

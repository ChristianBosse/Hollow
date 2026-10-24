//! Keeping process memory out of the pagefile (WIN-02).
//!
//! Every Hollow process installs [`LockedAlloc`] as its global allocator.
//! It locks the pages behind each allocation into RAM with `VirtualLock`, so
//! Windows can't write page contents, keys or Tor state to `pagefile.sys`,
//! and zeroes every block before freeing it.
//!
//! Limits, stated plainly: locking is best-effort and capped by the working
//! set reserved with [`reserve_lockable_memory`]; allocations beyond it stay
//! pageable (counted in [`LockStats`]). Locked memory is still written to
//! `hiberfil.sys` when the machine hibernates; Hollow Health warns about that.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::System::Memory::{
    QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_DISABLE, SetProcessWorkingSetSizeEx, VirtualLock,
};
use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::wide::last_error;

const PAGE: usize = 4096;
/// `Locked` bit of PSAPI_WORKING_SET_EX_BLOCK.
const WS_EX_LOCKED: usize = 1 << 22;

static LOCK_FAILURES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Last page range this thread locked, so runs of small allocations on
    /// the same pages don't each cost a system call.
    static LAST_LOCKED: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

/// Global allocator: the system heap, plus page locking and zero-on-free.
#[derive(Debug, Default, Clone, Copy)]
pub struct LockedAlloc;

// SAFETY: delegates allocation to `System`; locking and zeroing only touch
// memory inside blocks we own.
unsafe impl GlobalAlloc for LockedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        lock_pages(ptr, layout.size());
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        lock_pages(ptr, layout.size());
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` is a live block of `layout.size()` bytes.
        unsafe {
            std::ptr::write_bytes(ptr, 0, layout.size());
            System.dealloc(ptr, layout);
        }
    }

    // Not System.realloc: it could free the old block without zeroing it.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            return std::ptr::null_mut();
        };
        // SAFETY: standard realloc contract; both blocks are valid for the
        // copied length.
        unsafe {
            let new = self.alloc(new_layout);
            if !new.is_null() {
                std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            new
        }
    }
}

fn lock_pages(ptr: *mut u8, size: usize) {
    if ptr.is_null() || size == 0 {
        return;
    }
    let start = ptr as usize & !(PAGE - 1);
    let end = (ptr as usize).saturating_add(size).div_ceil(PAGE) * PAGE;
    let already = LAST_LOCKED.try_with(|last| {
        let (s, e) = last.get();
        s <= start && end <= e
    });
    if already == Ok(true) {
        return;
    }
    // SAFETY: the range lies inside committed heap pages we just allocated.
    if unsafe { VirtualLock(start as *const _, end - start) } == 0 {
        LOCK_FAILURES.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = LAST_LOCKED.try_with(|last| last.set((start, end)));
    }
}

/// Raises this process's minimum working set so up to `bytes` can be locked.
/// Call once, early in `main`.
pub fn reserve_lockable_memory(bytes: usize) -> io::Result<()> {
    let max = bytes.saturating_mul(2);
    // SAFETY: pseudo-handle for the current process; the flags make the
    // limits soft, so Windows can still trim us under real memory pressure
    // except for locked pages.
    let ok = unsafe {
        SetProcessWorkingSetSizeEx(
            GetCurrentProcess(),
            bytes,
            max,
            QUOTA_LIMITS_HARDWS_MIN_DISABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE,
        )
    };
    if ok == 0 { Err(last_error("SetProcessWorkingSetSizeEx")) } else { Ok(()) }
}

#[derive(Debug, Clone, Copy)]
pub struct LockStats {
    /// Allocations whose pages could not be locked (working-set quota reached).
    pub lock_failures: usize,
}

pub fn lock_stats() -> LockStats {
    LockStats { lock_failures: LOCK_FAILURES.load(Ordering::Relaxed) }
}

/// Whether the page holding `ptr` is currently locked in RAM, as Windows
/// reports it. Used by the self-test.
pub fn is_page_locked(ptr: *const u8) -> io::Result<bool> {
    let mut info = PSAPI_WORKING_SET_EX_INFORMATION {
        VirtualAddress: ptr as *mut _,
        // SAFETY: plain-data union; zero is a valid value.
        VirtualAttributes: unsafe { std::mem::zeroed() },
    };
    // SAFETY: one entry of the right size.
    let ok = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            (&mut info as *mut PSAPI_WORKING_SET_EX_INFORMATION).cast(),
            std::mem::size_of::<PSAPI_WORKING_SET_EX_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error("QueryWorkingSetEx"));
    }
    // SAFETY: the union's `Flags` view covers the whole block.
    let flags = unsafe { info.VirtualAttributes.Flags };
    Ok(flags & WS_EX_LOCKED != 0)
}

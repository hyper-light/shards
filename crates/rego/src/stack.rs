//! The stack a thread has left. A policy recurses as deep as its rules depend on one
//! another: Go grows a goroutine's stack for it (to 1 GB), while a Rust thread's stack
//! is fixed when the thread starts (the policy thread's 123 MiB, M126). What recurses on
//! a policy's rules asks [`enough`] first, and errs rather than overflow the stack,
//! which would end the process.

use std::cell::Cell;

/// The stack a builtin call takes at most: every call of the builtin corpus, the deepest
/// documents Go reads among them, ran on 205 KiB on aarch64-unknown-linux-gnu, the most
/// of the targets CI tests, release builds all (177 to 205 KiB; test-release's take 65 to
/// 129, M159), measured with tests/builtins.rs's `SHARDS_REGO_LEAF`, which holds every
/// target's release build to it.
pub const LEAF: usize = 205 << 10;

thread_local! {
    /// The lowest address this thread's stack reaches, once the OS has said; `usize::MAX`
    /// where it does not.
    static END: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Where this thread's stack is now.
#[inline(always)]
fn here() -> usize {
    let mark = 0u8;
    std::ptr::from_ref(&mark) as usize
}

/// The bytes of stack below the caller, where the OS says where the stack ends.
pub fn left() -> Option<usize> {
    let end = END.with(|e| match e.get() {
        Some(end) => end,
        None => {
            let end = platform::stack_end().unwrap_or(usize::MAX);
            e.set(Some(end));
            end
        }
    });
    if end == usize::MAX {
        return None;
    }
    here().checked_sub(end)
}

/// Whether `need` bytes of stack are left below the caller (true where the OS does not
/// say).
pub fn enough(need: usize) -> bool {
    left().is_none_or(|l| l >= need)
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn stack_end() -> Option<usize> {
        // SAFETY: pthread_self names the calling thread, whose stack's top and size the
        // two calls read.
        let (top, size) = unsafe {
            let t = libc::pthread_self();
            (
                libc::pthread_get_stackaddr_np(t) as usize,
                libc::pthread_get_stacksize_np(t),
            )
        };
        top.checked_sub(size)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn stack_end() -> Option<usize> {
        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        // SAFETY: pthread_getattr_np fills `attr` for the calling thread; on success it is
        // read, then destroyed once.
        unsafe {
            if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                return None;
            }
            let mut addr = std::ptr::null_mut();
            let mut size = 0;
            let r = libc::pthread_attr_getstack(attr.as_ptr(), &raw mut addr, &raw mut size);
            libc::pthread_attr_destroy(attr.as_mut_ptr());
            (r == 0).then_some(addr as usize)
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};

    /// The base of the reservation that holds this frame: the stack's lowest address.
    pub fn stack_end() -> Option<usize> {
        let mark = 0u8;
        let mut info = std::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::zeroed();
        // SAFETY: VirtualQuery describes the region holding `mark` into `info`, at most
        // its size, and returns how many bytes it wrote.
        let n = unsafe {
            VirtualQuery(
                std::ptr::from_ref(&mark).cast(),
                info.as_mut_ptr(),
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 {
            return None;
        }
        // SAFETY: VirtualQuery wrote the struct whole (n is its size).
        let info = unsafe { info.assume_init() };
        Some(info.AllocationBase as usize)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    pub fn stack_end() -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_knows_how_much_stack_it_has_left() {
        let size = 1 << 20;
        let left = std::thread::Builder::new()
            .stack_size(size)
            .spawn(super::left)
            .unwrap()
            .join()
            .unwrap();
        if cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            let left = left.unwrap();
            assert!(left > size / 2 && left <= size + (64 << 10), "{left}");
        }
    }
}

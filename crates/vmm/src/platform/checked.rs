//! Copies to and from memory whose pages may be gone (D189): device memory, a file's
//! mapping, whose file another process may cut short under it, or whose pages its disk may
//! fail to give back. The host's kernel answers a system call's access there with EFAULT;
//! the VMM's own access there would end its process with SIGBUS, or SIGSEGV where nothing
//! is mapped. So such a copy is code this process's handler of those signals knows: a fault
//! within it resumes where it returns that it failed, as Linux's own copies from user
//! memory resume (Documentation/arch/x86/exception-tables.rst), and any other fault goes on
//! to whatever handled the signal before, Rust's report of a stack overflow included, as
//! the access is made again.
//!
//! Against a pipe, the source written into it and read back out (POSIX: write(2) and
//! read(2) answer an inaccessible buffer with EFAULT), 1 MiB from a file's resident
//! mapping took 69 to 140 µs at p50 where memcpy took 19 to 30 on Fedora, and 99 to 115
//! where it took 13 to 14 on the Mac; Linux's process_vm_readv(2) of the process itself,
//! which macOS has not, 32 to 49 (PM M304).
//!
//! Elsewhere device memory is a pmem device's file alone, on Windows, whose size cannot
//! change while it is mapped (SetEndOfFile: "UnmapViewOfFile must be called to unmap all
//! views ... before you can call SetEndOfFile"): its copies are those of guest RAM.

use std::io;

/// Copies `len` bytes from `src` to `dst`: an error where an access faulted, `dst` written
/// as far as the copy reached.
///
/// # Safety
/// `src` and `dst` are `len` bytes each, apart, `dst` memory nothing else accesses
/// meanwhile; either may lie where nothing is mapped, or where an access faults.
#[cfg(all(unix, not(miri), any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn copy_checked(src: *const u8, dst: *mut u8, len: usize) -> io::Result<()> {
    faults::install()?;
    #[cfg(test)]
    super::copy::COPIED.with(|c| c.set(c.get() + len as u64));
    // SAFETY: as the caller promises; an access of the copy's that faults returns 1, its
    // handler installed above.
    if unsafe { faults::shards_checked_copy(dst, src, len) } == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EFAULT))
    }
}

/// Copies `len` bytes from `src` to `dst`, as guest RAM's are copied ([`super::copy_in`]):
/// here device memory does not fault (above), and Miri runs no `asm!`.
///
/// # Safety
/// `src` is valid for reads and `dst` for writes of `len` bytes, the two apart, and nothing
/// else accesses `dst` meanwhile.
#[cfg(not(all(unix, not(miri), any(target_arch = "aarch64", target_arch = "x86_64"))))]
pub unsafe fn copy_checked(src: *const u8, dst: *mut u8, len: usize) -> io::Result<()> {
    // SAFETY: as the caller promises.
    unsafe { super::copy::copy_in(src, dst, len) };
    Ok(())
}

#[cfg(all(unix, not(miri), any(target_arch = "aarch64", target_arch = "x86_64")))]
mod faults {
    use std::io;
    use std::sync::OnceLock;

    /// A symbol's name as the platform's C compiler writes it: Mach-O's with a leading
    /// underscore.
    #[cfg(target_vendor = "apple")]
    macro_rules! c_name {
        ($name:literal) => {
            concat!("_", $name)
        };
    }
    #[cfg(not(target_vendor = "apple"))]
    macro_rules! c_name {
        ($name:literal) => {
            $name
        };
    }

    /// The copy's section, its text's.
    #[cfg(target_vendor = "apple")]
    macro_rules! text {
        () => {
            ".pushsection __TEXT,__text,regular,pure_instructions"
        };
    }
    #[cfg(not(target_vendor = "apple"))]
    macro_rules! text {
        () => {
            ".pushsection .text"
        };
    }

    /// A symbol of the copy's, global within the program alone.
    #[cfg(target_vendor = "apple")]
    macro_rules! symbol {
        ($name:literal) => {
            concat!(
                ".globl ",
                c_name!($name),
                "\n.private_extern ",
                c_name!($name)
            )
        };
    }
    #[cfg(not(target_vendor = "apple"))]
    macro_rules! symbol {
        ($name:literal) => {
            concat!(".globl ", c_name!($name), "\n.hidden ", c_name!($name))
        };
    }

    /// The copy's fault's return, of the copy's own function: on Mach-O an alternate entry
    /// of its atom, which the linker then never moves apart from it.
    #[cfg(target_vendor = "apple")]
    macro_rules! within {
        ($name:literal) => {
            concat!(".alt_entry ", c_name!($name))
        };
    }
    #[cfg(not(target_vendor = "apple"))]
    macro_rules! within {
        ($name:literal) => {
            ""
        };
    }

    // x0 the destination, x1 the source, x2 the length; 64 bytes at a time by pairs of
    // 16-byte registers (v0 to v3, which a callee need not keep), then 16, then single
    // bytes. 0 in x0 once done; at a fault, the handler resumes at the fault's return.
    #[cfg(target_arch = "aarch64")]
    std::arch::global_asm!(
        text!(),
        symbol!("shards_checked_copy"),
        symbol!("shards_checked_copy_fault"),
        within!("shards_checked_copy_fault"),
        ".p2align 4",
        concat!(c_name!("shards_checked_copy"), ":"),
        "cmp x2, #64",
        "b.lo 2f",
        "1:",
        "ldp q0, q1, [x1]",
        "ldp q2, q3, [x1, #32]",
        "stp q0, q1, [x0]",
        "stp q2, q3, [x0, #32]",
        "add x1, x1, #64",
        "add x0, x0, #64",
        "sub x2, x2, #64",
        "cmp x2, #64",
        "b.hs 1b",
        "2:",
        "cmp x2, #16",
        "b.lo 4f",
        "3:",
        "ldr q0, [x1], #16",
        "str q0, [x0], #16",
        "sub x2, x2, #16",
        "cmp x2, #16",
        "b.hs 3b",
        "4:",
        "cbz x2, 6f",
        "5:",
        "ldrb w3, [x1], #1",
        "strb w3, [x0], #1",
        "subs x2, x2, #1",
        "b.ne 5b",
        "6:",
        "mov x0, #0",
        "ret",
        concat!(c_name!("shards_checked_copy_fault"), ":"),
        "mov x0, #1",
        "ret",
        ".popsection",
    );

    // rdi the destination, rsi the source, rdx the length; the direction flag clear on
    // entry (System V ABI). 0 in rax once done; at a fault, the handler resumes at the
    // fault's return.
    #[cfg(target_arch = "x86_64")]
    std::arch::global_asm!(
        text!(),
        symbol!("shards_checked_copy"),
        symbol!("shards_checked_copy_fault"),
        within!("shards_checked_copy_fault"),
        ".p2align 4",
        concat!(c_name!("shards_checked_copy"), ":"),
        "mov rcx, rdx",
        "rep movsb",
        "xor eax, eax",
        "ret",
        concat!(c_name!("shards_checked_copy_fault"), ":"),
        "mov eax, 1",
        "ret",
        ".popsection",
    );

    unsafe extern "C" {
        /// Copies `len` bytes from `src` to `dst`: 0, or 1 where an access faulted.
        pub(super) fn shards_checked_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
        /// Where a fault within [`shards_checked_copy`] resumes it: past its last access.
        fn shards_checked_copy_fault() -> usize;
    }

    /// The signals a fault raises, and what handled each before ours: what a fault not the
    /// copy's goes on to.
    const SIGNALS: [libc::c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];
    static BEFORE: [OnceLock<libc::sigaction>; 2] = [OnceLock::new(), OnceLock::new()];

    /// Has this process's faults within the copy resume it: once, each signal's handler
    /// before it kept for the faults that are not.
    pub(super) fn install() -> io::Result<()> {
        static INSTALLED: OnceLock<Result<(), i32>> = OnceLock::new();
        let installed = INSTALLED.get_or_init(|| {
            let errno = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL);
            for (&sig, before) in SIGNALS.iter().zip(&BEFORE) {
                // SAFETY: sigaction(2) reading a disposition into a zeroed one, then giving
                // the signal a handler with an empty mask.
                unsafe {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
                        return Err(errno());
                    }
                    let _ = before.set(old);
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = on_fault
                        as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
                        as libc::sighandler_t;
                    // On the thread's alternate stack where it has one, as Rust's report of
                    // a stack overflow, which a fault not the copy's goes on to, needs.
                    sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                    libc::sigemptyset(&mut sa.sa_mask);
                    if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                        return Err(errno());
                    }
                }
            }
            Ok(())
        });
        installed.map_err(io::Error::from_raw_os_error)
    }

    /// A fault within the copy resumes at its fault's return; any other, the signal's
    /// handler before ours handles, as the access is made again: as Rust's handler of a
    /// stack overflow passes on a fault not its own.
    extern "C" fn on_fault(sig: libc::c_int, _: *mut libc::siginfo_t, context: *mut libc::c_void) {
        let start = shards_checked_copy as *const () as usize as u64;
        let resume = shards_checked_copy_fault as *const () as usize as u64;
        // SAFETY: the kernel gives a SA_SIGINFO handler the interrupted thread's context.
        let pc = unsafe { pc(context) };
        if !pc.is_null() {
            // SAFETY: the context's program counter, which the kernel resumes the thread at.
            unsafe {
                if (start..resume).contains(&*pc) {
                    *pc = resume;
                    return;
                }
            }
        }
        let before = SIGNALS
            .iter()
            .zip(&BEFORE)
            .find(|&(&s, _)| s == sig)
            .and_then(|(_, b)| b.get());
        // SAFETY: sigaction(2), safe in a signal handler, of the disposition it gave, or a
        // zeroed one, the default's.
        unsafe {
            let default: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig, before.unwrap_or(&default), std::ptr::null_mut());
        }
    }

    /// Where `context`, a signal handler's, keeps the program counter of the thread it
    /// interrupted; null where it has none.
    ///
    /// # Safety
    /// `context` is a SA_SIGINFO handler's third argument.
    unsafe fn pc(context: *mut libc::c_void) -> *mut u64 {
        let uc = context.cast::<libc::ucontext_t>();
        if uc.is_null() {
            return std::ptr::null_mut();
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        // SAFETY: the caller's context; REG_RIP indexes its general registers.
        return unsafe {
            (*uc)
                .uc_mcontext
                .gregs
                .get_mut(libc::REG_RIP as usize)
                .map_or(std::ptr::null_mut(), |r| std::ptr::from_mut(r).cast())
        };
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        // SAFETY: the caller's context.
        return unsafe { (&raw mut (*uc).uc_mcontext.pc).cast() };
        #[cfg(target_vendor = "apple")]
        // SAFETY: the caller's context, whose machine state the kernel gives with it.
        return unsafe {
            let m = (*uc).uc_mcontext;
            if m.is_null() {
                return std::ptr::null_mut();
            }
            #[cfg(target_arch = "aarch64")]
            let at = &raw mut (*m).__ss.__pc;
            #[cfg(target_arch = "x86_64")]
            let at = &raw mut (*m).__ss.__rip;
            at
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file of `pages` pages mapped shared, read and written, and its descriptor.
    #[cfg(unix)]
    fn mapped(pages: usize) -> (shards_testdir::TempDir, std::fs::File, *mut u8, usize) {
        use std::os::fd::AsRawFd as _;
        let page = super::super::page_size().unwrap();
        let dir = shards_testdir::TempDir::new("checked").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join("f"))
            .unwrap();
        let len = pages * page;
        file.set_len(len as u64).unwrap();
        // SAFETY: a shared mapping of the file's `len` bytes, unmapped by the test's end.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(at, libc::MAP_FAILED);
        (dir, file, at.cast(), len)
    }

    #[test]
    fn a_checked_copy_copies_what_memcpy_does() {
        let src: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in [0, 1, 15, 16, 17, 63, 64, 65, 127, 128, 200, 999] {
            for off in [0, 1, 7] {
                let mut dst = vec![0xeeu8; 1001];
                let n = len.min(src.len() - off);
                // SAFETY: `n` bytes of each, apart.
                unsafe { copy_checked(src.as_ptr().add(off), dst.as_mut_ptr().add(1), n) }.unwrap();
                assert_eq!(&dst[1..1 + n], &src[off..off + n], "{len} from {off}");
                assert_eq!(dst[0], 0xee);
                assert!(dst[1 + n..].iter().all(|&b| b == 0xee), "{len} from {off}");
            }
        }
    }

    /// From a page of a file cut short under its mapping, where an access of the process's
    /// own takes SIGBUS, and to it, a checked copy fails and the process goes on; then from
    /// and to a page with no access, and to one that may only be read.
    #[cfg(all(unix, not(miri)))]
    #[test]
    fn a_checked_copy_fails_where_its_pages_are_gone() {
        let (_dir, file, at, len) = mapped(4);
        let page = len / 4;
        let mut buf = vec![7u8; page];
        // SAFETY: within the mapping, all of it the file's yet.
        unsafe { copy_checked(at.add(page), buf.as_mut_ptr(), page) }.unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        file.set_len(page as u64).unwrap();
        // SAFETY: within the mapping, past the file's end now: the copy faults.
        let from = unsafe { copy_checked(at.add(2 * page), buf.as_mut_ptr(), page) };
        assert_eq!(from.unwrap_err().raw_os_error(), Some(libc::EFAULT));
        // SAFETY: as above, the other way.
        let to = unsafe { copy_checked(buf.as_ptr(), at.add(3 * page), 16) };
        assert_eq!(to.unwrap_err().raw_os_error(), Some(libc::EFAULT));
        // A copy across the file's end fails, the file's part copied.
        let mut across = vec![9u8; 2 * page];
        // SAFETY: within the mapping.
        let r = unsafe { copy_checked(at, across.as_mut_ptr(), 2 * page) };
        assert!(r.is_err());
        assert!(across[..page].iter().all(|&b| b == 0));
        // SAFETY: the mapping's last page withheld, then its first made read-only.
        unsafe {
            assert_eq!(libc::mprotect(at.add(3 * page).cast(), page, libc::PROT_NONE), 0);
            assert_eq!(libc::mprotect(at.cast(), page, libc::PROT_READ), 0);
        }
        // SAFETY: within the mapping, faulting each.
        unsafe {
            assert!(copy_checked(at.add(3 * page), buf.as_mut_ptr(), 8).is_err());
            assert!(copy_checked(buf.as_ptr(), at.add(3 * page), 8).is_err());
            assert!(copy_checked(buf.as_ptr(), at, 8).is_err());
            copy_checked(at, buf.as_mut_ptr(), 8).unwrap();
        }
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(at.cast(), len) };
    }

    /// What a checked copy takes, against the copy guest RAM's take (`copy_in`), of 4 KiB,
    /// 64 KiB and 1 MiB from a file's shared mapping, every page of it resident, into the
    /// process's own memory, each timed (PM M304):
    ///
    ///     cargo test --profile test-release -p shards-vmm checked_copy_costs -- --ignored --nocapture
    #[cfg(all(unix, not(miri)))]
    #[test]
    #[ignore = "a measurement"]
    fn checked_copy_costs() {
        let (_dir, _file, at, len) = mapped((1 << 20) / super::super::page_size().unwrap());
        let mut dst = vec![1u8; len];
        // SAFETY: the mapping's `len` bytes, read whole so that each page is resident.
        unsafe { copy_checked(at, dst.as_mut_ptr(), len) }.unwrap();
        let summary = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            let at = |p: f64| v[((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
            format!(
                "n {} | p50 {:.2} | p90 {:.2} | p99 {:.2} | max {:.2} us",
                v.len(),
                at(0.5),
                at(0.9),
                at(0.99),
                v[v.len() - 1]
            )
        };
        for size in [4096, 65536, 1 << 20] {
            for checked in [false, true] {
                let times: Vec<f64> = (0..2000)
                    .map(|_| {
                        let t = std::time::Instant::now();
                        // SAFETY: `size` bytes of the mapping and of `dst`, apart.
                        unsafe {
                            if checked {
                                copy_checked(at, dst.as_mut_ptr(), size).unwrap();
                            } else {
                                super::super::copy_in(at, dst.as_mut_ptr(), size);
                            }
                        }
                        t.elapsed().as_secs_f64() * 1e6
                    })
                    .collect();
                let name = if checked { "checked" } else { "copy_in" };
                println!("{name} {size} B: {}", summary(times));
            }
        }
        // SAFETY: the mapping made by `mapped`.
        unsafe { libc::munmap(at.cast(), len) };
    }

    /// A fault outside a checked copy still ends its process as the signal's default has
    /// it: the handler before passes it on, as it passed it on before the copy's.
    #[cfg(all(unix, not(miri)))]
    #[test]
    fn a_fault_outside_a_checked_copy_still_ends_its_process() {
        faults::install().unwrap();
        let (_dir, _file, at, len) = mapped(1);
        // SAFETY: the mapping withheld, so that a read there faults.
        assert_eq!(unsafe { libc::mprotect(at.cast(), len, libc::PROT_NONE) }, 0);
        // SAFETY: the child does nothing but fault, or end at once.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: a read the mapping's protection faults.
            unsafe {
                std::ptr::read_volatile(at);
                libc::_exit(0);
            }
        }
        let mut status = 0;
        // SAFETY: waitpid(2) of the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFSIGNALED(status), "status {status:#x}");
        assert!(
            [libc::SIGSEGV, libc::SIGBUS].contains(&libc::WTERMSIG(status)),
            "status {status:#x}"
        );
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(at.cast(), len) };
    }
}

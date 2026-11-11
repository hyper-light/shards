use super::*;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};

fn smaps_for(address: usize) -> String {
    let text = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut selected = String::new();
    let mut inside = false;
    for line in text.lines() {
        if let Some((range, _)) = line.split_once(' ')
            && let Some((first, last)) = range.split_once('-')
            && let (Ok(first), Ok(last)) = (usize::from_str_radix(first, 16), usize::from_str_radix(last, 16))
        {
            inside = (first..last).contains(&address);
        }
        if inside {
            selected.push_str(line);
            selected.push('\n');
        }
    }
    selected
}

fn kilobytes(smaps: &str, field: &str) -> usize {
    smaps
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .unwrap()
        .split_ascii_whitespace()
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap()
        .checked_mul(1024)
        .unwrap()
}

#[test]
fn acquired_region_is_initialized_locked_writable_and_explicitly_released() {
    let original = b"native owner storage\0\xff";
    let mut region = Region::acquire_zeroed(NonZeroUsize::new(original.len()).unwrap()).unwrap();
    let extent = region.extent();
    assert_eq!(region.bytes().unwrap(), vec![0; original.len()]);
    region.bytes_mut().unwrap().copy_from_slice(original);
    assert_eq!(region.bytes().unwrap(), original);
    let address = region.bytes().unwrap().as_ptr() as usize;
    let smaps = smaps_for(address);
    let flags = smaps
        .lines()
        .find_map(|line| line.strip_prefix("VmFlags:"))
        .unwrap();
    assert!(flags.split_ascii_whitespace().any(|flag| flag == "lo"));
    assert!(flags.split_ascii_whitespace().any(|flag| flag == "dc"));
    assert!(!flags.split_ascii_whitespace().any(|flag| flag == "ex"));
    assert!(kilobytes(&smaps, "Rss:") >= extent.mapped.get());
    assert!(kilobytes(&smaps, "Locked:") >= extent.mapped.get());
    // The owner page follows the region as its own wipe-on-fork, unlocked VMA.
    let owner = smaps_for(address + extent.mapped.get());
    let owner_flags = owner
        .lines()
        .find_map(|line| line.strip_prefix("VmFlags:"))
        .unwrap();
    assert!(owner_flags.split_ascii_whitespace().any(|flag| flag == "wf"));
    assert!(!owner_flags.split_ascii_whitespace().any(|flag| flag == "lo"));
    // The fixture payload fits one actual page; this is an observation vector
    // size, not a production capacity or page-size default.
    assert_eq!(extent.mapped, extent.page);
    region.release().unwrap();
    let mut residency = [0u8];
    // SAFETY: mincore examines the now-unmapped numeric range without reading
    // it, writing at most one byte for its one-page fixture extent.
    let returned = unsafe {
        libc::mincore(
            address as *mut libc::c_void,
            extent.mapped.get(),
            residency.as_mut_ptr(),
        )
    };
    assert_eq!(returned, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ENOMEM));
    // SAFETY: as above, for the owner page that followed the region.
    let returned = unsafe {
        libc::mincore(
            (address + extent.mapped.get()) as *mut libc::c_void,
            extent.page.get(),
            residency.as_mut_ptr(),
        )
    };
    assert_eq!(returned, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ENOMEM));
    let _ = writeln!(
        std::io::stdout().lock(),
        "REGION usable={} mapped={} page={} locked/resident observed; owner page wf; explicit unmap then mincore ENOMEM",
        extent.usable,
        extent.mapped,
        extent.page
    );
}

#[test]
fn region_fork_guard_prevents_mapping_inheritance_and_reaps_the_probe() {
    let region = Region::acquire_zeroed(NonZeroUsize::MIN).unwrap();
    let address = region.bytes().unwrap().as_ptr();
    let extent = region.extent();
    assert_eq!(extent.mapped, extent.page);
    let mut pipe = [-1; 2];
    // SAFETY: pipe points at two initialized descriptor slots.
    let pipe_result = unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(pipe_result, 0);
    // SAFETY: each successful pipe2 descriptor is taken exactly once.
    let mut reader = unsafe { File::from_raw_fd(pipe[0]) };
    // SAFETY: this is the other fresh pipe2 descriptor.
    let writer = unsafe { File::from_raw_fd(pipe[1]) };
    // SAFETY: the child executes only the bounded raw-observation branch below,
    // never the test runtime, allocation, ordinary I/O, unwinding or destructors.
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        let refused = matches!(region.bytes(), Err(Failure::WrongProcess));
        let mut residency = [0u8];
        // SAFETY: mincore does not dereference the excluded numeric region and
        // has exactly one output byte for this one-page fixture extent.
        let result = unsafe {
            libc::mincore(
                address.cast_mut().cast(),
                extent.mapped.get(),
                residency.as_mut_ptr(),
            )
        };
        // SAFETY: Linux libc's thread-local errno pointer is valid in this child.
        let errno = unsafe { *libc::__errno_location() };
        let absent = result == -1 && errno == libc::ENOMEM;
        let report = [u8::from(refused), u8::from(absent)];
        // SAFETY: write a fixed initialized two-byte observation to our pipe.
        let written = unsafe { libc::write(writer.as_raw_fd(), report.as_ptr().cast(), report.len()) };
        // SAFETY: terminate this raw child without parent owners/destructors.
        unsafe {
            libc::_exit(if written == 2 {
                libc::EXIT_SUCCESS
            } else {
                libc::EXIT_FAILURE
            })
        };
    }
    drop(writer);
    // SAFETY: obtain the actual child's pidfd while it remains waitable here.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child, 0) };
    let pidfd = i32::try_from(pidfd).unwrap();
    assert!(pidfd >= 0);
    // SAFETY: pidfd_open returned this unique new descriptor.
    let pidfd = unsafe { File::from_raw_fd(pidfd) };
    let mut report = [0; 2];
    reader.read_exact(&mut report).unwrap();
    let mut poll = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd. The outer per-case deadline bounds a
    // broken fixture; no production timeout or periodic population probe exists.
    let ready = unsafe { libc::poll(&raw mut poll, 1, -1) };
    assert_eq!(ready, 1);
    let mut status = 0;
    // SAFETY: this exact child belongs only to this test's reaper.
    let waited = unsafe { libc::waitpid(child, &raw mut status, 0) };
    assert_eq!(waited, child);
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), libc::EXIT_SUCCESS);
    // SAFETY: checking the same already-reaped child cannot consume another.
    let again = unsafe { libc::waitpid(child, &raw mut status, libc::WNOHANG) };
    assert_eq!(again, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    assert_eq!(region.bytes().unwrap(), &[0]);
    region.release().unwrap();
    let _ = writeln!(
        std::io::stdout().lock(),
        "REGION_CHILD process_refused={} mapping_absent={} pidfd_ready=true waited=true ECHILD=true",
        report[0],
        report[1]
    );
    assert_eq!(
        report,
        [1, 1],
        "raw mincore observed a region inherited by the child"
    );
}

#[test]
fn unrepresentable_region_refuses_before_mapping_or_locking() {
    let failure = Region::acquire_zeroed(NonZeroUsize::new(usize::MAX).unwrap()).unwrap_err();
    assert!(matches!(failure, Rejected::Refused(Failure::Length)));
}

/// Writes one fixed observation and ends a raw child without destructors.
///
/// # Safety
/// Only for forked children of this module's tests; `out` is a pipe writer.
unsafe fn finish(out: i32, report: &[u8; 6]) -> ! {
    // SAFETY: an initialized six-byte observation into the test's own pipe.
    let written = unsafe { libc::write(out, report.as_ptr().cast(), report.len()) };
    // SAFETY: terminate this raw child without parent owners or destructors.
    unsafe {
        libc::_exit(if written == 6 {
            libc::EXIT_SUCCESS
        } else {
            libc::EXIT_FAILURE
        })
    }
}

/// The guest relation, unprivileged: this single-threaded child makes user
/// and PID namespaces; its child is PID 1 there (as guest init is) and
/// acquires the region, then clones a CLONE_NEWPID grandchild, also PID 1.
///
/// # Safety
/// Runs only raw system calls in forked children and always ends in `_exit`.
unsafe fn pid_namespace_owner(out: i32) -> ! {
    // SAFETY: plain flags; this freshly forked child has a single thread.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) } != 0 {
        // SAFETY: Linux libc's thread-local errno pointer is valid here.
        let errno = unsafe { *libc::__errno_location() };
        // SAFETY: as above, the test pipe writer.
        unsafe { finish(out, &[1, u8::try_from(errno).unwrap_or(u8::MAX), 0, 0, 0, 0]) }
    }
    // SAFETY: the next child is the new namespace's PID 1 and runs raw calls.
    let owner = unsafe { libc::fork() };
    if owner < 0 {
        // SAFETY: as above.
        unsafe { finish(out, &[3, 0, 0, 0, 0, 0]) }
    }
    if owner == 0 {
        // SAFETY: getpid has no pointer argument or ownership side effect.
        let owner_pid_one = u8::from(unsafe { libc::getpid() } == 1);
        let Ok(region) = Region::acquire_zeroed(NonZeroUsize::MIN) else {
            // SAFETY: as above.
            unsafe { finish(out, &[2, owner_pid_one, 0, 0, 0, 0]) }
        };
        let extent = region.extent();
        let Ok(address) = region
            .bytes()
            .map(|bytes| bytes.as_ptr().cast_mut().cast::<libc::c_void>())
        else {
            // SAFETY: as above.
            unsafe { finish(out, &[2, owner_pid_one, 0, 0, 0, 0]) }
        };
        // SAFETY: fork-like clone (no new stack, no CLONE_VM) into a new PID
        // namespace; the child runs only raw calls and ends in `finish`.
        let child = unsafe {
            libc::syscall(
                libc::SYS_clone,
                (libc::CLONE_NEWPID | libc::SIGCHLD) as libc::c_ulong,
                0usize,
                0usize,
                0usize,
                0usize,
            )
        };
        if child < 0 {
            // SAFETY: as above.
            unsafe { finish(out, &[3, owner_pid_one, 0, 0, 0, 0]) }
        }
        if child == 0 {
            // SAFETY: getpid has no pointer argument or ownership side effect.
            let child_pid_one = u8::from(unsafe { libc::getpid() } == 1);
            let refused = u8::from(matches!(region.bytes(), Err(Failure::WrongProcess)));
            // The excluded region is absent here: map this child's own page
            // where it was, then drop the copied owner over it.
            // SAFETY: NOREPLACE never clobbers; the range is page-aligned.
            let own = unsafe {
                libc::mmap(
                    address,
                    extent.mapped.get(),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                )
            };
            let mapped_own = u8::from(own == address);
            drop(region);
            let mut residency = [0u8];
            // SAFETY: mincore does not dereference the range; one output byte
            // covers this one-page fixture extent.
            let present = unsafe { libc::mincore(address, extent.mapped.get(), residency.as_mut_ptr()) } == 0;
            // SAFETY: as above.
            unsafe {
                finish(
                    out,
                    &[
                        0,
                        owner_pid_one,
                        child_pid_one,
                        refused,
                        mapped_own,
                        u8::from(present),
                    ],
                )
            }
        }
        let mut status = 0;
        // SAFETY: this exact child belongs only to this owner.
        let waited = unsafe { libc::waitpid(child as libc::pid_t, &raw mut status, 0) };
        let _ = region.release();
        // SAFETY: raw child exit carrying the grandchild's outcome.
        unsafe {
            libc::_exit(if waited > 0 && libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                1
            })
        }
    }
    let mut status = 0;
    // SAFETY: this exact child belongs only to this raw parent.
    let waited = unsafe { libc::waitpid(owner, &raw mut status, 0) };
    // SAFETY: raw child exit carrying the namespace owner's outcome.
    unsafe {
        libc::_exit(if waited > 0 && libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            1
        })
    }
}

#[test]
fn region_guard_holds_for_a_pid_one_child_in_a_new_pid_namespace() {
    let mut pipe = [-1; 2];
    // SAFETY: pipe points at two initialized descriptor slots.
    assert_eq!(unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: each successful pipe2 descriptor is taken exactly once.
    let mut reader = unsafe { File::from_raw_fd(pipe[0]) };
    // SAFETY: this is the other fresh pipe2 descriptor.
    let writer = unsafe { File::from_raw_fd(pipe[1]) };
    // SAFETY: the child executes only the raw branch above and always _exits.
    let outer = unsafe { libc::fork() };
    assert!(outer >= 0);
    if outer == 0 {
        // SAFETY: freshly forked single-threaded child; writer is our pipe.
        unsafe { pid_namespace_owner(writer.as_raw_fd()) }
    }
    drop(writer);
    let mut report = Vec::new();
    reader.read_to_end(&mut report).unwrap();
    let mut status = 0;
    // SAFETY: this exact child belongs only to this test's reaper.
    assert_eq!(unsafe { libc::waitpid(outer, &raw mut status, 0) }, outer);
    if report.first() == Some(&1) {
        let _ = writeln!(
            std::io::stdout().lock(),
            "SKIP: unprivileged user and PID namespaces are unavailable (errno {})",
            report.get(1).copied().unwrap_or(0)
        );
        return;
    }
    let _ = writeln!(std::io::stdout().lock(), "REGION_PIDNS report={report:?}");
    assert_eq!(report.len(), 6, "{report:?}");
    assert_eq!(report[0], 0, "fixture failed before observing: {report:?}");
    assert_eq!(report[1..3], [1, 1], "owner and child are both PID 1: {report:?}");
    assert_eq!(
        report[4], 1,
        "the excluded region left its address free in the child: {report:?}"
    );
    assert_eq!(
        report[3], 1,
        "a PID 1 child in a new namespace borrowed the excluded region: {report:?}"
    );
    assert_eq!(
        report[5], 1,
        "dropping the copied owner unmapped the child's own allocation: {report:?}"
    );
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == libc::EXIT_SUCCESS);
}

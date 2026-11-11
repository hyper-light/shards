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
            && let (Ok(first), Ok(last)) = (
                usize::from_str_radix(first, 16),
                usize::from_str_radix(last, 16),
            )
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
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOMEM)
    );
    let _ = writeln!(
        std::io::stdout().lock(),
        "REGION usable={} mapped={} page={} locked/resident observed; explicit unmap then mincore ENOMEM",
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
        let written =
            unsafe { libc::write(writer.as_raw_fd(), report.as_ptr().cast(), report.len()) };
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
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
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

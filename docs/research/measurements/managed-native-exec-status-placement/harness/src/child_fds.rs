//! Exact Linux-guest child FD placement, shared by init and native workers.
//! The caller supplies authority; this module only prepares/installs those objects.

use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inheritance {
    RetainAcrossExec,
    SetupOnly,
}

pub struct Binding<'a> {
    pub source: BorrowedFd<'a>,
    pub destination: RawFd,
    pub inheritance: Inheritance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Duplicate { destination: RawFd },
    Install { destination: RawFd },
    Close { first: u32, last: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Destination,
    Order,
    Capacity,
    Native {
        operation: Operation,
        errno: Option<i32>,
    },
}

struct Staged {
    source: OwnedFd,
    destination: RawFd,
    inheritance: Inheritance,
}

/// The caller supplies already-authorized objects in increasing destination order.
/// Preparation duplicates them above every destination before the child may exist.
pub struct PreparedFdMap {
    staged: Vec<Staged>,
}

impl PreparedFdMap {
    pub fn prepare(bindings: &[Binding<'_>]) -> Result<Self, Failure> {
        let mut previous = None;
        for binding in bindings {
            if binding.destination < 0 {
                return Err(Failure::Destination);
            }
            if previous.is_some_and(|fd| fd >= binding.destination) {
                return Err(Failure::Order);
            }
            previous = Some(binding.destination);
        }
        let first_free = match previous {
            Some(fd) => fd.checked_add(1).ok_or(Failure::Destination)?,
            None => 0,
        };
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(bindings.len())
            .map_err(|_| Failure::Capacity)?;
        for binding in bindings {
            // SAFETY: fcntl duplicates the held source above all destinations. The
            // duplicate remains owned here until parent cleanup/child installation.
            let fd = unsafe { libc::fcntl(binding.source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, first_free) };
            if fd < 0 {
                return Err(native(Operation::Duplicate {
                    destination: binding.destination,
                }));
            }
            staged.push(Staged {
                // SAFETY: successful F_DUPFD_CLOEXEC created a descriptor owned here alone.
                source: unsafe { OwnedFd::from_raw_fd(fd) },
                destination: binding.destination,
                inheritance: binding.inheritance,
            });
        }
        Ok(Self { staged })
    }

    /// Native duplicates retained by preparation. Original sources remain the
    /// caller's; parent admission must account for both sets during overlap.
    pub fn descriptors(&self) -> usize {
        self.staged.len()
    }

    /// Borrows the already-staged object without duplicating it. Capture any raw
    /// value before entering child-only install/closure; those operations invalidate
    /// the child's staged copies. Never borrow them again after that point.
    pub fn staged_source(&self, destination: RawFd) -> Option<BorrowedFd<'_>> {
        self.staged
            .iter()
            .find(|binding| binding.destination == destination)
            .map(|binding| binding.source.as_fd())
    }

    /// # Safety
    /// Only a single-threaded trusted post-clone child may call this. It closes
    /// all unlisted descriptors, including inherited Rust owners' copies. The
    /// child must exec or _exit on every outcome without running those destructors.
    pub unsafe fn install_in_child(&self) -> Result<(), Failure> {
        for binding in &self.staged {
            let flags = match binding.inheritance {
                Inheritance::RetainAcrossExec => 0,
                Inheritance::SetupOnly => libc::O_CLOEXEC,
            };
            // SAFETY: prepare staged every source above every destination; none
            // can be overwritten by this loop. Caller supplies the child-only context.
            if unsafe { libc::dup3(binding.source.as_raw_fd(), binding.destination, flags) } < 0 {
                return Err(native(Operation::Install {
                    destination: binding.destination,
                }));
            }
        }
        let mut next = 0u32;
        for binding in &self.staged {
            let destination = u32::try_from(binding.destination).map_err(|_| Failure::Destination)?;
            if next < destination {
                close(next, destination.checked_sub(1).ok_or(Failure::Destination)?)?;
            }
            next = destination.checked_add(1).ok_or(Failure::Destination)?;
        }
        close(next, u32::MAX)
    }
}

fn close(first: u32, last: u32) -> Result<(), Failure> {
    // SAFETY: the caller is the trusted child-only install path; these ranges
    // exclude exactly the installed descriptors and include all staging copies.
    if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } != 0 {
        return Err(native(Operation::Close { first, last }));
    }
    Ok(())
}

fn native(operation: Operation) -> Failure {
    // SAFETY: on the Linux glibc/musl guest targets, __errno_location returns
    // this thread's live errno slot. Copy it before another syscall can change it.
    let errno = unsafe { *libc::__errno_location() };
    Failure::Native {
        operation,
        errno: Some(errno),
    }
}

impl std::fmt::Debug for PreparedFdMap {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("PreparedFdMap")
            .field("descriptors", &self.staged.len())
            .finish()
    }
}

impl std::fmt::Debug for Binding<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Binding")
            .field("destination", &self.destination)
            .field("inheritance", &self.inheritance)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod exec_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: pipe2 writes a two-element descriptor array, owned below.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let [read, write] = fds;
        // SAFETY: successful pipe2 gave this test both newly owned descriptors.
        unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) }
    }

    fn read(fd: OwnedFd) -> Vec<u8> {
        let mut bytes = Vec::new();
        std::fs::File::from(fd).read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn exact_map_closes_surplus_separates_output_and_reaps_the_fixture() {
        let tmp = shards_testdir::TempDir::new("native-fd-map").unwrap();
        let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        {
            std::fs::write(tmp.join("input"), b"in").unwrap();
            std::fs::write(tmp.join("surplus"), b"must not inherit").unwrap();
            let input = std::fs::File::open(tmp.join("input")).unwrap();
            let surplus = std::fs::File::open(tmp.join("surplus")).unwrap();
            let (out_read, out_write) = pipe();
            let (err_read, err_write) = pipe();
            let (report_read, report_write) = pipe();
            // Targets0..4 are the explicit fixture map. Put the forbidden descriptor
            // above all targets so this test cannot confuse it with a replacement.
            // SAFETY: fcntl duplicates this owned disposable fixture descriptor.
            let forbidden = unsafe { libc::fcntl(surplus.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
            assert!(forbidden >= 5);
            // SAFETY: successful fcntl just transferred ownership to this test.
            let forbidden = unsafe { OwnedFd::from_raw_fd(forbidden) };
            let forbidden_raw = forbidden.as_raw_fd();
            let map = PreparedFdMap::prepare(&[
                Binding {
                    source: input.as_fd(),
                    destination: 0,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: out_write.as_fd(),
                    destination: 1,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: err_write.as_fd(),
                    destination: 2,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: report_write.as_fd(),
                    destination: 3,
                    inheritance: Inheritance::RetainAcrossExec,
                },
                Binding {
                    source: input.as_fd(),
                    destination: 4,
                    inheritance: Inheritance::SetupOnly,
                },
            ])
            .unwrap();
            assert_eq!(map.descriptors(), 5);
            // SAFETY: the child uses only the prepared raw map and async-signal-safe
            // descriptor calls before _exit; it runs no inherited Rust destructors.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                // SAFETY: this is the trusted post-fork fixture child. Every branch
                // ends with _exit, including an installation failure.
                unsafe {
                    let mut input_bytes = [0; 2];
                    let placed = map.install_in_child().is_ok();
                    let setup_flags = libc::fcntl(4, libc::F_GETFD);
                    let report_flags = libc::fcntl(3, libc::F_GETFD);
                    let success = placed
                        && libc::fcntl(forbidden_raw, libc::F_GETFD) == -1
                        && setup_flags >= 0
                        && setup_flags & libc::FD_CLOEXEC != 0
                        && report_flags >= 0
                        && report_flags & libc::FD_CLOEXEC == 0
                        && libc::read(0, input_bytes.as_mut_ptr().cast(), input_bytes.len()) == 2
                        && input_bytes == *b"in"
                        && libc::write(1, c"out".as_ptr().cast(), c"out".to_bytes_with_nul().len()) == 4
                        && libc::write(2, b"err\xff".as_ptr().cast(), 4) == 4
                        && libc::write(3, b"ok".as_ptr().cast(), 2) == 2;
                    libc::_exit(if success { 0 } else { 1 });
                }
            }
            // Parent staging/writer copies must retire too for real pipe EOF.
            drop(map);
            drop(out_write);
            drop(err_write);
            drop(report_write);
            let mut status = 0;
            // SAFETY: this unreaped direct child remains owned by this test.
            assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            assert_eq!(read(out_read), b"out\0");
            assert_eq!(read(err_read), b"err\xff");
            assert_eq!(read(report_read), b"ok");
        }
        assert_eq!(std::fs::read_dir("/proc/self/fd").unwrap().count(), before);
    }

    #[test]
    fn invalid_destination_lists_fail_before_any_duplicate_is_owned() {
        let tmp = shards_testdir::TempDir::new("native-fd-invalid").unwrap();
        let source = std::fs::File::open(&tmp).unwrap();
        let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        for destinations in [[0, 0], [1, 0]] {
            let bindings = destinations.map(|destination| Binding {
                source: source.as_fd(),
                destination,
                inheritance: Inheritance::RetainAcrossExec,
            });
            assert!(matches!(PreparedFdMap::prepare(&bindings), Err(Failure::Order)));
        }
        for destination in [-1, i32::MAX] {
            let binding = Binding {
                source: source.as_fd(),
                destination,
                inheritance: Inheritance::RetainAcrossExec,
            };
            assert!(matches!(
                PreparedFdMap::prepare(&[binding]),
                Err(Failure::Destination)
            ));
        }
        assert_eq!(PreparedFdMap::prepare(&[]).unwrap().descriptors(), 0);
        assert_eq!(std::fs::read_dir("/proc/self/fd").unwrap().count(), before);
    }

    #[test]
    fn staged_sources_serve_preplacement_ready_and_raw_actor_retirement() {
        fn identity(fd: RawFd) -> (libc::dev_t, libc::ino_t) {
            // SAFETY: an initialized native stat buffer for this held test object.
            let mut value: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: fstat writes only the live buffer for a held descriptor.
            let result = unsafe { libc::fstat(fd, &raw mut value) };
            assert_eq!(result, 0);
            (value.st_dev, value.st_ino)
        }
        let reap = |pid| {
            let mut status = 0;
            // SAFETY: this test retains the actual unreaped direct child.
            let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
            assert_eq!(waited, pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            // SAFETY: the same exact child has already been reaped.
            let again = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            assert_eq!(again, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        };
        let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        {
            // Existing protected reaper roles Owner/Gate/Split/Ready/Binary at3..7.
            // Distinct valid pipes make a wrong-role accessor observable, without
            // injecting a syscall failure or executing the namespace reaper.
            const READY: RawFd = 6;
            let endpoints = [3, 4, 5, READY, 7].map(|destination| (destination, pipe()));
            let bindings: Vec<_> = endpoints
                .iter()
                .map(|(destination, (_, write))| Binding {
                    source: write.as_fd(),
                    destination: *destination,
                    inheritance: Inheritance::RetainAcrossExec,
                })
                .collect();
            let map = PreparedFdMap::prepare(&bindings).unwrap();
            let retained = std::fs::read_dir("/proc/self/fd").unwrap().count();
            let mut staged = [0; 5];
            for ((slot, (destination, (_, original))), binding) in
                staged.iter_mut().zip(&endpoints).zip(&bindings)
            {
                let source = map.staged_source(*destination).unwrap();
                assert_ne!(source.as_raw_fd(), original.as_raw_fd());
                assert_eq!(identity(source.as_raw_fd()), identity(binding.source.as_raw_fd()));
                *slot = source.as_raw_fd();
            }
            assert!(map.staged_source(-1).is_none());
            assert!(map.staged_source(READY + 2).is_none());
            assert_eq!(map.descriptors(), endpoints.len());
            assert_eq!(std::fs::read_dir("/proc/self/fd").unwrap().count(), retained);
            let original_ready = endpoints
                .iter()
                .find(|(destination, _)| *destination == READY)
                .unwrap()
                .1
                .1
                .as_raw_fd();
            let staged_ready = map.staged_source(READY).unwrap().as_raw_fd();
            // SAFETY: all raw values were captured before clone; this disposable
            // child only closes its own staged copies, uses the original, and _exits.
            let actor = unsafe { libc::fork() };
            assert!(actor >= 0);
            if actor == 0 {
                // SAFETY: independent child FD table; no allocation or inherited
                // destructor runs after these closes invalidate its map copies.
                unsafe {
                    for fd in staged {
                        if libc::close(fd) != 0 {
                            libc::_exit(1);
                        }
                    }
                    for fd in staged {
                        if libc::fcntl(fd, libc::F_GETFD) != -1 || *libc::__errno_location() != libc::EBADF {
                            libc::_exit(2);
                        }
                    }
                    if libc::fcntl(original_ready, libc::F_GETFD) < 0
                        || libc::write(original_ready, b"A".as_ptr().cast(), 1) != 1
                    {
                        libc::_exit(3);
                    }
                    libc::_exit(0);
                }
            }
            reap(actor);
            // SAFETY: the second ordinary raw child reports through the exact
            // staged Ready, installs the SAME map, then uses installed Ready.
            let reaper = unsafe { libc::fork() };
            assert!(reaper >= 0);
            if reaper == 0 {
                // SAFETY: fixed buffers and actual held descriptors only; exec/_exit
                // contract forbids destructors after install closes staging.
                unsafe {
                    if libc::write(staged_ready, b"P".as_ptr().cast(), 1) != 1
                        || map.install_in_child().is_err()
                    {
                        libc::_exit(4);
                    }
                    if libc::fcntl(staged_ready, libc::F_GETFD) != -1
                        || *libc::__errno_location() != libc::EBADF
                        || libc::write(READY, b"I".as_ptr().cast(), 1) != 1
                    {
                        libc::_exit(5);
                    }
                    libc::_exit(0);
                }
            }
            reap(reaper);
            drop(map);
            drop(bindings);
            for (destination, (reader, writer)) in endpoints {
                drop(writer);
                if destination == READY {
                    assert_eq!(read(reader), b"API");
                }
            }
        }
        assert_eq!(std::fs::read_dir("/proc/self/fd").unwrap().count(), before);
    }
}

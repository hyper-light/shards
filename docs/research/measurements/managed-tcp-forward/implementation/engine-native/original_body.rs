// Frozen original engine post-HELLO copy body, cbd392/e2e4e6e6.
// Harness adapter supplies owned Unix/File endpoints; VSOCK and HELLO are not exercised.
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub(super) fn relay(conn: UnixStream, host: std::fs::File) {
    std::thread::scope(|s| {
        let up = std::thread::Builder::new()
            .name("engine up".into())
            .spawn_scoped(s, || {
                let _ = io::copy(&mut &conn, &mut &host);
                // SAFETY: shutdown(2) of a descriptor this thread borrows.
                unsafe { libc::shutdown(host.as_raw_fd(), libc::SHUT_WR) };
            });
        if up.is_ok() {
            let _ = io::copy(&mut &host, &mut &conn);
        }
        // Wakes the up thread's read, which then ends (Linux unix_shutdown).
        let _ = conn.shutdown(std::net::Shutdown::Both);
    });
}

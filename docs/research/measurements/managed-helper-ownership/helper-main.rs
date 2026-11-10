//! Native fixture entry only; sandbox.rs and server::quote are exact source copies.
mod sandbox;
mod server;

fn main() -> std::process::ExitCode {
    let fd: i32 = std::env::args().nth(1).unwrap().parse().unwrap();
    // SAFETY: the fixture passes this owned control socket and starts no threads.
    unsafe {
        assert_eq!(libc::dup2(fd, sandbox::CONTROL_FD), sandbox::CONTROL_FD);
        assert_eq!(libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0), 0);
    }
    sandbox::helper()
}

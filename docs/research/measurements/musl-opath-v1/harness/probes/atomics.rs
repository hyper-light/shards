
/// Probe (docs/research/measurements/musl-opath-v1): appended to crates/server/src/sandbox.rs
/// for one run, then taken away.
#[cfg(test)]
mod probe {
    #![allow(clippy::unwrap_used)]
    use std::os::fd::AsRawFd as _;

    /// What a program's timed Atomics.wait comes to in a worker's runtime (`run`): QuickJS-NG
    /// blocks there only where its runtime may block (`JS_SetCanBlock`), which rquickjs
    /// leaves unset, so it throws rather than wait on a futex.
    #[test]
    fn a_program_cannot_wait_on_atomics() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let came = super::run(
            theirs.as_raw_fd(),
            "const a = new Int32Array(new SharedArrayBuffer(4)); return Atomics.wait(a, 0, 0, 50);",
        );
        drop(ours);
        eprintln!("ATOMICS_WAIT {came:?}");
        assert!(matches!(&came, Err(e) if e.contains("cannot block")), "{came:?}");
    }
}

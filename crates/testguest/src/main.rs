//! shards-testguest: PID 1 of E2E test VMs. Runs the test named by `shards_test=` on
//! the kernel command line, prints `SHARDS-TEST PASS` or `SHARDS-TEST FAIL <why>`, and
//! powers the VM off.

#[cfg(target_os = "linux")]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "shards-testguest only runs inside a Linux guest"
        );
        std::process::exit(1);
    }
}

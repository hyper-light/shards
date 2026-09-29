//! shards-testguest: PID 1 of E2E test VMs. Runs the test named by `shards_test=` on
//! the kernel command line, prints `SHARDS-TEST PASS` or `SHARDS-TEST FAIL <why>`, and
//! powers the VM off. Run as any other process, it is a workload for images (workload.rs).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod workload;

fn main() {
    #[cfg(target_os = "linux")]
    if std::process::id() == 1 {
        linux::main();
    } else {
        workload::main();
    }
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

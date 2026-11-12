//! shards-testguest: PID 1 of E2E test VMs. Runs the test named by `shards_test=` on
//! the kernel command line, prints `SHARDS-TEST PASS` or `SHARDS-TEST FAIL <why>`, and
//! powers the VM off. Run as any other process, it is a workload for images (workload.rs).

#[cfg(target_os = "linux")]
mod cleanup;
#[cfg(target_os = "linux")]
mod engine_bulk;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod managed_lifecycle;
#[cfg(target_os = "linux")]
mod managed_limits;
#[cfg(target_os = "linux")]
mod managed_network;
#[cfg(target_os = "linux")]
mod managed_reap;
#[cfg(target_os = "linux")]
mod workload;

fn main() {
    #[cfg(target_os = "linux")]
    // The VM's init, which the kernel starts with `shards_test=` from its command line in
    // its environment; anything else, PID 1 of a build step's namespace included, is a
    // workload.
    if std::process::id() == 1 && std::env::var_os("shards_test").is_some() {
        linux::main();
    } else if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "managed-network")
    {
        std::process::exit(managed_network::main());
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

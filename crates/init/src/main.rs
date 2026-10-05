//! shards-init: PID 1 of every shards guest.

#[cfg(target_os = "linux")]
mod build;
#[cfg(target_os = "linux")]
mod changes;
#[cfg(target_os = "linux")]
mod copy;
#[cfg(target_os = "linux")]
mod defaults;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod frames;
#[cfg(target_os = "linux")]
mod inroot;
#[cfg(target_os = "linux")]
mod layer;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod net;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod orders;
#[cfg(target_os = "linux")]
mod procs;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod tree;

fn main() {
    // `shards-init processes`, as any process but PID 1: the guest's process dump
    // (procs.rs), for the tests of `top`.
    #[cfg(target_os = "linux")]
    if std::process::id() != 1 && std::env::args_os().nth(1).is_some_and(|a| a == "processes") {
        let code = i32::from(procs::print().is_err());
        std::process::exit(code);
    }
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "shards-init only runs as PID 1 inside a Linux guest"
        );
        std::process::exit(1);
    }
}

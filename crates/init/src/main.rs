//! shards-init: PID 1 of every shards guest.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod run;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod user;

fn main() {
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

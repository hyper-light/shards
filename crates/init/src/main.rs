//! shards-init: PID 1 of every shards guest.

#[cfg(target_os = "linux")]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("shards-init only runs as PID 1 inside a Linux guest");
        std::process::exit(1);
    }
}

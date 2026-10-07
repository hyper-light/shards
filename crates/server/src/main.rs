//! shards-server, the in-VM server (AGENTFILE_ARCH.md §5, §12 answer 18; docs/design/
//! architecture.md D60): one instance for each agent or harness, which shards-init starts
//! outside every domain, least-privileged, from the read-only device it lies on.

#[cfg(target_os = "linux")]
mod server;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    return server::main();
    #[cfg(not(target_os = "linux"))]
    {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "shards-server runs in a shards guest alone");
        std::process::ExitCode::FAILURE
    }
}

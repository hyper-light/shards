use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;

fn main() -> std::io::Result<()> {
    // Exact stdio map plus the one descriptor read_dir opens to inspect it.
    // This executable receives no status, held image/workdir or helper FD.
    if std::fs::read_dir("/proc/self/fd")?.count() != 4 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 1 {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let environment: Vec<_> = std::env::vars_os().collect();
    if environment.len() != 1 || environment[0].0 != "ONLY" {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let directory = std::env::current_dir()?;
    let mut stdout = std::io::stdout().lock();
    for bytes in [
        arguments[0].as_os_str().as_bytes(),
        &[b"ONLY=".as_slice(), environment[0].1.as_os_str().as_bytes()].concat(),
        &input,
        directory.as_os_str().as_bytes(),
    ] {
        stdout.write_all(bytes)?;
        stdout.write_all(&[0])?;
    }
    stdout.flush()?;
    std::io::stderr().write_all(b"stderr\0\xff")
}

//! Other CPUs' programs, run under their emulators (D179): the emulators the host names,
//! registered with this kernel's binfmt_misc as Docker's binfmt tool registers them with a
//! host's (shards_abi::emulate), each opened as it is registered (flag F), so that every
//! root and mount namespace reaches it. A builder registers them as its host asks
//! (build.rs); a run, from its kernel's command line, before its image's root is mounted.

use std::io::{self, Write};
use std::path::Path;

fn err(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// Registers the emulators of `architectures` from the emulator image mounted at `image`,
/// through binfmt_misc mounted at `binfmt`, mounted there first where it is not.
pub(crate) fn register(image: &Path, binfmt: &Path, architectures: &[&str]) -> io::Result<()> {
    let register = binfmt.join("register");
    if !register.exists() {
        crate::build::mount("binfmt_misc", binfmt, "binfmt_misc", 0, "")?;
    }
    for arch in architectures {
        let e = shards_abi::emulate::of(arch).ok_or_else(|| err(format!("no emulator runs {arch}")))?;
        let path = image.join("usr/bin").join(e.binary);
        let path = path
            .to_str()
            .ok_or_else(|| err(format!("{}: its path is not UTF-8", e.binary)))?;
        let line = shards_abi::emulate::register_line(e, path);
        // The line in one write, as binfmt_misc reads one (fs/binfmt_misc.c,
        // bm_register_write).
        let mut f = std::fs::OpenOptions::new().write(true).open(&register)?;
        match f.write(line.as_bytes()) {
            Ok(n) if n == line.len() => {}
            Ok(_) => return Err(err(format!("registering {}: a short write", e.binary))),
            Err(x) => return Err(io::Error::new(x.kind(), format!("registering {}: {x}", e.binary))),
        }
    }
    Ok(())
}

/// A run's emulators, which `shards_emulate=DEVICE:ARCH[,ARCH...]` on the kernel command
/// line names: the emulator image on pmem `DEVICE` mounted in the initramfs, binfmt_misc
/// beside it, and each emulator registered, before the run's root moves over the
/// initramfs, which then keeps both mounts out of every process's reach. Both stay mounted:
/// an entry goes with its binfmt_misc instance's inodes (fs/binfmt_misc.c,
/// bm_evict_inode). None where the line names none.
pub(crate) fn for_run() -> io::Result<()> {
    let Some(given) = std::env::var_os("shards_emulate") else {
        return Ok(());
    };
    let given = given.to_str().ok_or_else(|| err("shards_emulate is not UTF-8"))?;
    let (device, architectures) = given
        .split_once(':')
        .ok_or_else(|| err("shards_emulate without its device"))?;
    for dir in ["/emulators", "/binfmt"] {
        std::fs::create_dir_all(dir)?;
    }
    crate::pmem::mount_erofs(device, "/emulators", "dax=always", None)
        .map_err(|e| io::Error::new(e.kind(), format!("mounting {device} on /emulators: {e}")))?;
    let architectures: Vec<&str> = architectures.split(',').filter(|a| !a.is_empty()).collect();
    register(Path::new("/emulators"), Path::new("/binfmt"), &architectures)
}

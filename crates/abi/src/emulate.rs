//! Another CPU's programs in a microVM (D179): the emulators a microVM's own kernel runs
//! them under, through binfmt_misc, as Docker's binfmt tool registers them with a host's
//! (tonistiigi/binfmt e29e7d7, cmd/binfmt/config.go and main.go `install`; the magic and
//! masks are QEMU's, scripts/qemu-binfmt-conf.sh): each `qemu-<cpu>` of the emulator
//! image, by the OCI architecture it runs. The host chooses the ones a microVM needs; its
//! init registers them.

use alloc::format;
use alloc::string::String;

/// One emulator: what it runs, and how binfmt_misc knows its programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emulator {
    /// The OCI architecture it runs (`amd64`, `arm64`, `arm`, …).
    pub architecture: &'static str,
    /// Its file in the emulator image's `/usr/bin`, and its binfmt_misc entry's name.
    pub binary: &'static str,
    /// The ELF header bytes of its CPU's programs, and the mask they are matched under,
    /// as binfmt_misc's register line takes them: `\xHH` escapes, which the kernel reads.
    pub magic: &'static str,
    pub mask: &'static str,
}

pub const EMULATORS: &[Emulator] = &[
    Emulator {
        architecture: "amd64",
        binary: "qemu-x86_64",
        magic: r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00",
        mask: r"\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
    Emulator {
        architecture: "arm64",
        binary: "qemu-aarch64",
        magic: r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
    Emulator {
        architecture: "arm",
        binary: "qemu-arm",
        magic: r"\x7fELF\x01\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x28\x00",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
    Emulator {
        architecture: "s390x",
        binary: "qemu-s390x",
        magic: r"\x7fELF\x02\x02\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x16",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff",
    },
    Emulator {
        architecture: "ppc64le",
        binary: "qemu-ppc64le",
        magic: r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x15\x00",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\x00",
    },
    Emulator {
        architecture: "riscv64",
        binary: "qemu-riscv64",
        magic: r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xf3\x00",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
    Emulator {
        architecture: "386",
        binary: "qemu-i386",
        magic: r"\x7fELF\x01\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x03\x00",
        mask: r"\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
    Emulator {
        architecture: "mips64le",
        binary: "qemu-mips64el",
        magic: r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x08\x00",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\x00\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
    Emulator {
        architecture: "mips64",
        binary: "qemu-mips64",
        magic: r"\x7fELF\x02\x02\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x08",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\x00\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff",
    },
    Emulator {
        architecture: "loong64",
        binary: "qemu-loongarch64",
        magic: r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x02\x01",
        mask: r"\xff\xff\xff\xff\xff\xff\xff\xfc\x00\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff",
    },
];

/// The emulator that runs `architecture`'s programs, if there is one.
pub fn of(architecture: &str) -> Option<&'static Emulator> {
    EMULATORS.iter().find(|e| e.architecture == architecture)
}

/// Its binfmt_misc register line, as Docker's tool writes it: matched by magic at offset
/// 0, the interpreter at `path`, with flags CFP: credentials and security of the program
/// run, not the emulator's (C, which opens the program for it, O); the emulator opened as
/// it is registered, so that every root and mount namespace reaches it (F); and argv[0]
/// kept, which the emulator image's builds expect (P: its `QEMU_PRESERVE_ARGV0=1`).
pub fn register_line(e: &Emulator, path: &str) -> String {
    format!(":{}:M:0:{}:{}:{path}:CFP", e.binary, e.magic, e.mask)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The line Docker's tool writes for x86_64 (cmd/binfmt/main.go `install`, with the
    /// image's `QEMU_PRESERVE_ARGV0` set), byte for byte; no emulator for a CPU it has none
    /// of.
    #[test]
    fn a_register_line_is_dockers_tools() {
        let e = of("amd64").unwrap();
        assert_eq!(
            register_line(e, "/usr/bin/qemu-x86_64"),
            r":qemu-x86_64:M:0:\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00:\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/usr/bin/qemu-x86_64:CFP"
        );
        assert_eq!(of("sparc64"), None);
        assert_eq!(EMULATORS.len(), 10);
    }
}

//! The keys of a spec's setup entries ([`Spec::setup`](super::Spec::setup)), the run
//! protocol's one registry of them (D257). An entry is its key alone (`readonly`), or its
//! key, `=` and its value (`tmpfs=/t\0rw`): its key is its bytes before its first `=`.
//!
//! shards-init refuses a spec holding an entry of a key not here before it applies any of
//! it ([`check`]), naming the key, so a key one build's host writes and another's init has
//! no reader for is refused, never ignored. And as this registry is of this crate's sources,
//! a key added here gives the contract another identity (D28), by which the host refuses an
//! init of another build before it sends any spec.

/// Every key, sorted, so that [`registered`] finds one by binary search: each one a host
/// composer writes or shards-init reads (D257's census).
pub const KEYS: &[&[u8]] = &[
    b"address",
    b"address6",
    b"caps",
    b"cgroupns",
    b"cgroups-rw",
    b"confine-eth0",
    b"devices",
    b"dns",
    b"domain-volume",
    b"domains-seccomp",
    b"domains-seccomp-none",
    b"endpoint-sysctl",
    b"engine",
    b"engine-made",
    b"engine-ring",
    b"gpus",
    b"group",
    b"hosts-file",
    b"image-env",
    b"in-join",
    b"in-member",
    b"init",
    b"ipc",
    b"join",
    b"join-layer",
    b"join-volume",
    b"link-local",
    b"mac",
    b"managed-active",
    b"managed-capacity",
    b"managed-input",
    b"managed-input-token",
    b"managed-members",
    b"managed-token",
    b"member",
    b"member-accepts",
    b"member-arg",
    b"member-ends",
    b"member-env",
    b"member-file",
    b"member-terminal",
    b"member-workdir",
    b"nesting",
    b"net",
    b"network",
    b"nnp",
    b"oom",
    b"own-bind",
    b"own-net",
    b"own-net-none",
    b"own-publish",
    b"pid",
    b"plug-root",
    b"privileged",
    b"readonly",
    b"ready-by",
    b"rt",
    b"run-ca",
    b"run-ca-host",
    b"rw-size",
    b"seccomp",
    b"seccomp-binds",
    b"seccomp-mounts",
    b"shm",
    b"sockets",
    b"swap",
    b"sysctl",
    b"tmpfs",
    b"ulimit",
    b"umask",
    b"unmasked",
    b"uplink6",
    b"uts",
    b"volume",
];

/// `entry`'s key: its bytes before its first `=`, else all of it.
pub fn key(entry: &[u8]) -> &[u8] {
    entry.split(|&b| b == b'=').next().unwrap_or(entry)
}

/// Whether `key` is one of the run protocol's.
pub fn registered(key: &[u8]) -> bool {
    KEYS.binary_search(&key).is_ok()
}

/// A setup entry of a key the run protocol does not register (D257), by its key alone:
/// never its value, which may be a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unregistered<'a> {
    pub key: &'a [u8],
}

impl core::fmt::Display for Unregistered<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "a setup entry of key \"{}\", which the run protocol does not register",
            self.key.escape_ascii()
        )
    }
}

/// `setup`, refused at its first entry of a key the run protocol does not register.
pub fn check(setup: &[alloc::vec::Vec<u8>]) -> Result<(), Unregistered<'_>> {
    for entry in setup {
        let key = key(entry);
        if !registered(key) {
            return Err(Unregistered { key });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// The registry is sorted and each key once, as `registered`'s binary search needs; and
    /// each key is a key: not empty, no `=` (where a value starts) and no NUL.
    #[test]
    fn the_registry_is_sorted_and_each_a_key() {
        for pair in KEYS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} before {:?}",
                pair[0].escape_ascii(),
                pair[1].escape_ascii()
            );
        }
        for k in KEYS {
            assert!(
                !k.is_empty() && !k.contains(&b'=') && !k.contains(&0),
                "{:?}",
                k.escape_ascii()
            );
        }
    }

    /// Every key this crate defines an entry by, and every entry its functions make, is
    /// registered: what the guest and the host share of setup is here (D28).
    #[test]
    fn every_entry_this_crate_makes_is_registered() {
        use super::super::{managed, members, nesting, plug, socket, trust};
        let constants: [&[u8]; 22] = [
            members::ENV_KEY,
            members::FILE_KEY,
            members::WORKDIR_KEY,
            members::IMAGE_ENV_KEY,
            members::ENDS,
            members::TERMINAL_KEY,
            members::ARG_KEY,
            members::EXEC_KEY,
            managed::CAPACITY_KEY,
            managed::TOKEN_KEY,
            managed::MEMBERS_KEY,
            managed::ACTIVE_KEY,
            managed::INPUT_KEY,
            managed::INPUT_TOKEN_KEY,
            nesting::GRANT,
            nesting::READY_BY,
            nesting::GPUS,
            socket::DIR,
            trust::CA_KEY,
            trust::HOST_KEY,
            plug::ROOT_KEY,
            crate::ring::SPEC,
        ];
        for c in constants {
            assert!(registered(key(c)), "{:?}", c.escape_ascii());
        }
        let made: Vec<Vec<u8>> = vec![
            members::env(0, b"K=v").unwrap(),
            members::file(0, 0o600, b"f", b"x").unwrap(),
            members::workdir(0, b"/w").unwrap(),
            members::image_env(b"K=v").unwrap(),
            members::terminal(0),
            members::arg(0, b"--resume").unwrap(),
            members::exec_in(0),
            socket::grant(b"/run/s").unwrap(),
            trust::ca(b"-----BEGIN CERTIFICATE-----").unwrap(),
            trust::host(b"api.example.com").unwrap(),
        ];
        assert_eq!(check(&made), Ok(()));
    }

    /// An entry's key is what comes before its first `=`, or the whole entry; one of an
    /// unregistered key is refused by its key, the first such, and its value never shown.
    #[test]
    fn an_entry_of_an_unregistered_key_is_refused_by_its_key() {
        assert_eq!(key(b"tmpfs=/t\0rw"), b"tmpfs");
        assert_eq!(key(b"sysctl=net.ipv4.ip_forward=1"), b"sysctl");
        assert_eq!(key(b"pid=joiner=7"), b"pid");
        assert_eq!(key(b"readonly"), b"readonly");
        assert_eq!(key(b""), b"");
        let ok = vec![
            b"readonly".to_vec(),
            b"tmpfs=/t\0rw".to_vec(),
            b"pid=host".to_vec(),
        ];
        assert_eq!(check(&ok), Ok(()));
        assert_eq!(check(&[]), Ok(()));
        for (bad, refused) in [
            (&b"bogus=sk-secret"[..], &b"bogus"[..]),
            (b"bogus", b"bogus"),
            (b"", b""),
            (b"readonly2", b"readonly2"),
            (b"Readonly", b"Readonly"),
            (b"=readonly", b""),
        ] {
            let setup = vec![b"readonly".to_vec(), bad.to_vec(), b"other=x".to_vec()];
            assert_eq!(
                check(&setup),
                Err(Unregistered { key: refused }),
                "{:?}",
                bad.escape_ascii()
            );
        }
        let said = alloc::format!("{}", check(&[b"bogus=sk-secret".to_vec()]).unwrap_err());
        assert!(
            said.contains("\"bogus\"") && !said.contains("sk-secret"),
            "{said}"
        );
    }
}

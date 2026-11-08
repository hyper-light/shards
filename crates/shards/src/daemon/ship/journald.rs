//! The journald driver's sending (D166): each entry (logging/journald.rs) a datagram to
//! the journal's socket, as go-systemd's journal.Send sends one; an entry too long for a
//! datagram in a sealed memfd passed with an empty one, as systemd's own sd_journal_sendv
//! passes it (sd-journal journal-send.c), which journald takes from any sender, where
//! go-systemd's is a file it makes in /dev/shm. The journal's socket is systemd's, or the
//! one `SHARDS_JOURNAL_SOCKET` names. A datagram on a local socket is in the journal's
//! queue as it is sent: delivered.

use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

use super::{Driver, Message, Wait};
use crate::logging::journald::{self, From};

/// The journal's LineMax where its configuration sets none: "Defaults to 48K"
/// (journald.conf(5); systemd src/journal/journald-config.c `DEFAULT_LINE_MAX`, 48*1024).
const DEFAULT_LINE_MAX: usize = 48 * 1024;
/// The least LineMax the journal takes: "If the value is very small, it is raised to 79"
/// (journald.conf(5)).
const LEAST_LINE_MAX: usize = 79;

/// The most of a line one entry carries: the LineMax this host's journal is configured
/// with, where it cuts any program's output into entries as it takes it from a stream
/// (journald.conf(5)), read as journald reads its configuration (systemd.syntax(7),
/// "configuration directories and precedence"): its main file, the first of `/etc`, `/run`,
/// `/usr/local/lib` and `/usr/lib`'s, then its drop-ins (`journald.conf.d/*.conf`) by name
/// across them, a name in an earlier directory hiding the same in a later, each setting the
/// last read; its default where none sets it. So a run's long line is in the journal as the
/// journal keeps any program's (PM M494), a longer one in parts joined by
/// CONTAINER_PARTIAL's fields. Read as each shipper starts.
fn line_max(root: &Path) -> usize {
    let dirs = ["etc", "run", "usr/local/lib", "usr/lib"].map(|d| root.join(d).join("systemd"));
    let mut files = Vec::new();
    if let Some(main) = dirs.iter().map(|d| d.join("journald.conf")).find(|f| f.is_file()) {
        files.push(main);
    }
    let mut drop_ins = std::collections::BTreeMap::new();
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("journald.conf.d")) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            if std::path::Path::new(&name)
                .extension()
                .is_some_and(|x| x == "conf")
            {
                drop_ins.entry(name).or_insert_with(|| e.path());
            }
        }
    }
    files.extend(drop_ins.into_values());
    let mut most = DEFAULT_LINE_MAX;
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut journal = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                journal = line == "[Journal]";
            } else if journal
                && let Some((key, value)) = line.split_once('=')
                && key.trim() == "LineMax"
                && let Some(n) = size(value.trim())
            {
                most = n.max(LEAST_LINE_MAX);
            }
        }
    }
    most
}

/// A size as systemd's configuration writes one (parse_size, base 1024): digits, then
/// `K`, `M`, `G` or `T`, or none; none where it is not one.
fn size(value: &str) -> Option<usize> {
    let (digits, unit) = value.split_at(value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len()));
    let shift = match unit.trim() {
        "" | "B" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return None,
    };
    digits
        .parse::<usize>()
        .ok()?
        .checked_mul(1usize.checked_shl(shift)?)
}

pub(super) struct Journald {
    from: From,
    socket: PathBuf,
    conn: Option<UnixDatagram>,
    /// The entries sent: the next one's ordinal less one (moby's journald.ordinal).
    sent: u64,
    /// The host's journal's LineMax ([`line_max`]).
    line_max: usize,
}

impl Journald {
    pub(super) fn new(from: From) -> Journald {
        let socket = std::env::var_os("SHARDS_JOURNAL_SOCKET")
            .map_or_else(|| PathBuf::from(journald::SOCKET), PathBuf::from);
        Journald {
            from,
            socket,
            conn: None,
            sent: 0,
            line_max: line_max(Path::new("/")),
        }
    }

    fn write(&mut self, entry: &[u8]) -> io::Result<()> {
        if self.conn.is_none() {
            let s = UnixDatagram::unbound()?;
            s.connect(&self.socket)?;
            self.conn = Some(s);
        }
        let Some(conn) = &self.conn else {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        };
        let sent = match super::sockets::send_datagram(conn, entry) {
            Err(e) if super::sockets::too_large(&e) => send_sealed(conn, entry),
            sent => sent,
        };
        if sent.is_err() {
            self.conn = None;
        }
        sent
    }
}

impl Driver for Journald {
    fn part(&self) -> usize {
        self.line_max
    }

    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        let ordinal = self.sent.saturating_add(1);
        let entry = journald::entry(&self.from, m.stream, m.at, m.line, m.part, ordinal);
        self.write(&entry)?;
        self.sent = ordinal;
        Ok(())
    }
}

/// Sends `entry` to the journal in a memfd, sealed so that it can change no more, passed
/// with an empty datagram.
fn send_sealed(conn: &UnixDatagram, entry: &[u8]) -> io::Result<()> {
    // SAFETY: memfd_create(2) of a C string's name.
    let fd = unsafe {
        libc::memfd_create(
            c"shards-journal".as_ptr(),
            libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is new, and this function's alone.
    let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
    std::fs::File::from(memfd.try_clone()?).write_all(entry)?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    // SAFETY: fcntl(2) F_ADD_SEALS on a memfd this function holds.
    if unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = std::mem::size_of::<libc::c_int>();
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(bytes as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: an all-zero msghdr is valid; the control buffer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is aligned (u64s) and CMSG_SPACE(bytes) long, so the
    // first header and its descriptor fit.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for the memfd"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
        libc::CMSG_DATA(cmsg)
            .cast::<libc::c_int>()
            .write_unaligned(memfd.as_raw_fd());
    }
    loop {
        // SAFETY: sendmsg(2) of an initialized msghdr over live buffers, on a socket the
        // caller holds.
        if unsafe { libc::sendmsg(conn.as_raw_fd(), &msg, 0) } >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Do: a root with no journald configuration; then a main file in /usr/lib and in
    /// /etc, drop-ins in /etc, /run and /usr/lib, one name in two of them, a setting of
    /// another section, and one too small. Expect the default; then each setting as journald
    /// reads them: /etc's main file over /usr/lib's, drop-ins by name after it, /etc's hiding
    /// /usr/lib's of the same name; the other section's ignored; the least raised to 79.
    #[test]
    fn line_max_is_read_as_journald_reads_its_configuration() {
        let root = shards_testdir::TempDir::new("jconf").unwrap();
        assert_eq!(line_max(&root), 48 * 1024);
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("usr/lib/systemd/journald.conf", "[Journal]\nLineMax=1K\n");
        write(
            "etc/systemd/journald.conf",
            "[Journal]\n#LineMax=48K\nLineMax = 2K\n",
        );
        assert_eq!(line_max(&root), 2048);
        write(
            "usr/lib/systemd/journald.conf.d/50-a.conf",
            "[Journal]\nLineMax=3K\n",
        );
        write("run/systemd/journald.conf.d/60-b.conf", "[Journal]\nLineMax=1M\n");
        assert_eq!(line_max(&root), 1 << 20);
        // /etc's 60-b hides /run's; another section's setting is no journal's.
        write(
            "etc/systemd/journald.conf.d/60-b.conf",
            "[Other]\nLineMax=9K\n[Journal]\nLineMax=5000\n",
        );
        assert_eq!(line_max(&root), 5000);
        write(
            "etc/systemd/journald.conf.d/70-c.conf",
            "[Journal]\nLineMax=10\nLineMax=oops\n",
        );
        assert_eq!(line_max(&root), 79);
        assert_eq!(
            (size("48K"), size("2G"), size("x"), size("1Q")),
            (Some(48 << 10), Some(2 << 30), None, None)
        );
    }
}

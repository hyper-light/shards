//! Shared test transport for authenticated owner-private canonical capture.
#![allow(dead_code)] // Separate integration binaries use different fixture operations.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use hyper_quic::{ClientConfig, StreamId};
use sha2::{Digest as _, Sha256};
use shards_abi::run::managed::{Channel, Event as CaptureEvent, InputEnd, Record, StreamState};
use shards_remote::frame::{self, Tag};
use shards_remote::identity::{Fingerprint, Identity};
use shards_remote::managed::{self, Admitted, Reason, RunId, Scope};
use shards_remote::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};

/// What an observer's or an input writer's wait is held to.
#[derive(Clone, Copy)]
pub(super) enum Until<'a> {
    /// Its caller's own deadline: the coordinator's campaigns' explicit phase deadlines, their
    /// caller's input, and harness_bringup's none at all.
    At(Instant),
    /// Judged as the tests' waits are (D209): on the daemon of a home and its microVMs
    /// (`common::Patience::of`), its kind its caller's call site; its driver given no deadline.
    Judged(&'a Path),
    /// No bound: what it waits for is owed by an event of this process's own, its driver's
    /// end once the test cancels or stops it (the fixture's own unit tests, with no daemon).
    Ordered,
}

impl From<Instant> for Until<'static> {
    fn from(at: Instant) -> Self {
        Until::At(at)
    }
}

impl Until<'_> {
    /// Its driver's deadline: its caller's, or none where its waits are judged.
    fn deadline(self) -> Option<Instant> {
        match self {
            Until::At(at) => Some(at),
            Until::Judged(_) | Until::Ordered => None,
        }
    }
}

/// `rx`'s next item as `until` holds its wait: within its caller's deadline, or while its
/// home's daemon and microVMs go on, a timeout once its judge refuses it (its account on
/// stderr, `Patience::holds`).
#[track_caller]
fn receive<T>(rx: &mpsc::Receiver<T>, until: Until<'_>) -> Result<T, mpsc::RecvTimeoutError> {
    match until {
        Until::At(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
        Until::Ordered => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        Until::Judged(home) => {
            let mut patience = crate::common::Patience::of(home);
            loop {
                match rx.recv_timeout(patience.next_look()) {
                    Ok(item) => {
                        patience.done();
                        return Ok(item);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if patience.holds() => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

pub(super) enum Event {
    Record(u64, Vec<u8>),
    Lost(Option<u64>),
    Failed(String),
    End,
}

impl std::fmt::Debug for Event {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Record(offset, bytes) => out
                .debug_struct("Record")
                .field("offset", offset)
                .field("bytes", &bytes.len())
                .finish(),
            Self::Lost(code) => out.debug_tuple("Lost").field(code).finish(),
            Self::Failed(error) => out.debug_tuple("Failed").field(error).finish(),
            Self::End => out.write_str("End"),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) enum ControlReceipt {
    Write { accepted: usize, pending: usize },
    Read { bytes: usize, ended: bool },
    Finish { error: Option<String> },
    WriteError(String),
}

pub(super) enum Ask {
    Connect(SocketAddr),
    Ack(u64),
    AckAndFinish(u64),
    Cancel,
    /// Panics its driver's thread, for a test of how a lost stream says its driver ended.
    #[cfg(test)]
    Panic,
    #[cfg(test)]
    RawControl {
        bytes: Vec<u8>,
        finish: bool,
        receipts: mpsc::Sender<ControlReceipt>,
    },
}

struct Client {
    config: Option<ClientConfig>,
    scope: Scope,
    conn: Option<ConnKey>,
    stream: Option<StreamId>,
    events: mpsc::Sender<Event>,
    pending: Vec<u8>,
    #[cfg(test)]
    control_receipts: Option<mpsc::Sender<ControlReceipt>>,
    inbound: Vec<u8>,
    finish: bool,
    done: bool,
    deadline: Option<Instant>,
    window: usize,
}

impl Client {
    fn fail(&mut self, message: String) {
        let _ = self.events.send(Event::Failed(message));
        self.done = true;
    }

    fn flush(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        while !self.pending.is_empty() {
            match io.write(conn, stream, &self.pending) {
                Ok(0) => return,
                Ok(n) => {
                    self.pending.drain(..n);
                    #[cfg(test)]
                    if let Some(receipts) = &self.control_receipts {
                        let _ = receipts.send(ControlReceipt::Write {
                            accepted: n,
                            pending: self.pending.len(),
                        });
                    }
                }
                Err(error) => {
                    #[cfg(test)]
                    if let Some(receipts) = &self.control_receipts {
                        let _ = receipts.send(ControlReceipt::WriteError(error.to_string()));
                    }
                    self.fail(error.to_string());
                    return;
                }
            }
        }
        if self.finish {
            let result = io.finish(conn, stream);
            #[cfg(test)]
            if let Some(receipts) = &self.control_receipts {
                let _ = receipts.send(ControlReceipt::Finish {
                    error: result.as_ref().err().map(ToString::to_string),
                });
            }
            if let Err(error) = result {
                self.fail(error.to_string());
            }
            self.finish = false;
        }
    }
}

impl Handler for Client {
    type Command = Ask;

    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
        self.stream = Some(io.open(conn).map_err(|_| Close {
            code: 1,
            why: "fixture stream failed",
        })?);
        self.pending.extend_from_slice(&managed::subscribe(self.scope, 0));
        self.flush(io);
        Ok(())
    }

    fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let (bytes, ended) = match io.read_within(conn, stream, self.window) {
                Ok(Read::Bytes(bytes)) => (bytes, false),
                Ok(Read::End(bytes)) => (bytes, true),
                Ok(Read::Blocked) => return,
                other => {
                    self.fail(format!("managed read {other:?}"));
                    return;
                }
            };
            #[cfg(test)]
            if let Some(receipts) = &self.control_receipts {
                let _ = receipts.send(ControlReceipt::Read {
                    bytes: bytes.len(),
                    ended,
                });
            }
            self.inbound.extend_from_slice(&bytes);
            loop {
                let (tag, bytes) = match frame::take(&mut self.inbound) {
                    Ok(Some(record)) => record,
                    Ok(None) => break,
                    Err(error) => {
                        self.fail(error.to_string());
                        return;
                    }
                };
                if tag != Tag::Stdout {
                    self.fail("unexpected managed data tag".into());
                    return;
                }
                let Some((offset, payload)) = bytes.split_first_chunk::<8>() else {
                    self.fail("missing managed offset".into());
                    return;
                };
                let _ = self
                    .events
                    .send(Event::Record(u64::from_be_bytes(*offset), payload.to_vec()));
            }
            if ended {
                let _ = self.events.send(Event::End);
                return;
            }
        }
    }

    fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.fail("stream stopped".into());
    }
    fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, _: &str, code: Option<u64>) {
        let _ = self.events.send(Event::Lost(code));
        self.done = true;
    }

    fn command(&mut self, io: &mut Io<'_>, command: Ask) {
        match command {
            // An observer connects once: its client's config goes with its first Connect.
            Ask::Connect(address) => match self.config.take().map(|config| io.connect(address, config)) {
                Some(Ok(conn)) => self.conn = Some(conn),
                Some(Err(error)) => self.fail(error.to_string()),
                None => self.fail("an observer connects once".into()),
            },
            #[cfg(test)]
            Ask::Panic => panic!("an observer's driver asked to panic"),
            Ask::AckAndFinish(offset) => {
                self.pending.extend_from_slice(&managed::consumed(offset));
                self.finish = true;
                self.flush(io);
            }
            Ask::Ack(offset) => {
                self.pending.extend_from_slice(&managed::consumed(offset));
                self.flush(io);
            }
            Ask::Cancel => {
                if let Some(conn) = self.conn {
                    io.close(conn, 0, "ordinary fixture observer cancel");
                }
                self.done = true;
            }
            #[cfg(test)]
            Ask::RawControl {
                bytes,
                finish,
                receipts,
            } => {
                if bytes.len() > managed::CONTROL_BYTES
                    || self.control_receipts.is_some()
                    || !self.pending.is_empty()
                {
                    self.fail("raw control requires one bounded command after flushed subscription".into());
                    return;
                }
                self.control_receipts = Some(receipts);
                self.pending.extend_from_slice(&bytes);
                self.finish |= finish;
                self.flush(io);
            }
        }
    }

    fn done(&self) -> bool {
        self.done
    }
    fn due(&self) -> Option<Instant> {
        self.deadline
    }
    fn timeout(&mut self, _: &mut Io<'_>, _: Instant) {
        self.fail("observer deadline".into());
    }
}

/// A socket an owner observer opened, by the kernel's identity for it, with the descriptor
/// number it had. A process-wide descriptor count also moves with every test running beside
/// it; this identity is this observer's socket alone, which no other socket takes while
/// the kernel holds either: Linux's inode, by which /proc/self/fd and /proc/net/udp name it;
/// macOS's protocol control block generation, libproc's `insi_gencnt` (sys/proc_info.h,
/// "generation count of this instance"), which xnu's in_pcballoc takes from its protocol's
/// PCB table as it raises that table's count, so that no two of the table's PCBs share one.
/// Windows names a socket by nothing a test can read again ([`UNIDENTIFIED`]).
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Held {
    pub(super) fd: i32,
    pub(super) id: u64,
}

/// Why a Windows fixture leaves its observers' sockets' return unverified.
#[cfg(windows)]
pub(super) const UNIDENTIFIED: &str = "Windows names a socket by nothing a test can read again: no inode or \
     protocol control block generation, a handle value is reused once closed, and a handle \
     WSADuplicateSocket made holds the socket under another; its address alone, which another socket \
     may take once it is free, would read a socket taken again as one not returned";

/// This process's sockets: each inode with the descriptors that hold it.
#[cfg(target_os = "linux")]
fn sockets() -> BTreeMap<u64, Vec<i32>> {
    let mut held = BTreeMap::<u64, Vec<i32>>::new();
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        let entry = entry.unwrap();
        // A descriptor closed since the listing has no link left to read.
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(inode) = target
            .to_str()
            .and_then(|t| t.strip_prefix("socket:["))
            .and_then(|t| t.strip_suffix(']'))
        else {
            continue;
        };
        let fd = entry.file_name().to_str().unwrap().parse().unwrap();
        held.entry(inode.parse().unwrap()).or_default().push(fd);
    }
    held
}

/// This network namespace's IPv4 UDP sockets, each local address with its inode
/// (/proc/net/udp, whose address is the `__be32` printed as a host-order integer).
#[cfg(target_os = "linux")]
fn udp_sockets() -> Vec<(std::net::SocketAddrV4, u64)> {
    let table = std::fs::read_to_string("/proc/net/udp").unwrap();
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let (address, port) = fields.get(1)?.split_once(':')?;
            let ip = std::net::Ipv4Addr::from(u32::from_str_radix(address, 16).ok()?.to_ne_bytes());
            let port = u16::from_str_radix(port, 16).ok()?;
            Some((
                std::net::SocketAddrV4::new(ip, port),
                fields.get(9)?.parse().ok()?,
            ))
        })
        .collect()
}

/// This process's IPv4 UDP sockets: each one's generation with the descriptors that hold it.
#[cfg(target_os = "macos")]
fn sockets() -> BTreeMap<u64, Vec<i32>> {
    let mut held = BTreeMap::<u64, Vec<i32>>::new();
    let own = libc::pid_t::try_from(std::process::id()).unwrap();
    let ours = libproc::udp(own)
        .unwrap_or_else(|| panic!("libproc on this process: {}", std::io::Error::last_os_error()));
    for (_, id, fd) in ours {
        held.entry(id).or_default().push(fd);
    }
    held
}

/// The IPv4 UDP sockets of every process this user may look at, each local address with its
/// generation: as /proc/net/udp lists a socket that a descriptor of any process holds, one
/// another process took (a child that inherited it) among them. A process of another user,
/// which libproc tells nothing of, holds none of this one's sockets unless passed it.
#[cfg(target_os = "macos")]
fn udp_sockets() -> Vec<(std::net::SocketAddrV4, u64)> {
    // Once a socket, as /proc/net/udp lists it, however many descriptors hold it.
    let listed: std::collections::BTreeSet<_> = libproc::processes()
        .into_iter()
        .filter_map(libproc::udp)
        .flatten()
        .map(|(address, id, _)| (address, id))
        .collect();
    listed.into_iter().collect()
}

/// What libproc (/usr/lib/libproc.dylib) says of a process's descriptors and sockets, laid out
/// as the macOS SDK's sys/proc_info.h lays out its replies.
#[cfg(target_os = "macos")]
mod libproc {
    use std::net::{Ipv4Addr, SocketAddrV4};

    // sys/proc_info.h's numbers: the flavor asking for a socket's state, the kind an internet
    // socket's union holds, and an IPv4 PCB's flag.
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    const SOCKINFO_IN: libc::c_int = 1;
    const INI_IPV4: u8 = 0x1;

    #[repr(C)]
    struct ProcFileinfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: libc::off_t,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    struct SockbufInfo {
        sbi_cc: u32,
        sbi_hiwat: u32,
        sbi_mbcnt: u32,
        sbi_mbmax: u32,
        sbi_lowat: u32,
        sbi_flags: i16,
        sbi_timeo: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct In4in6Addr {
        i46a_pad32: [u32; 3],
        i46a_addr4: libc::in_addr,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union InAddr {
        ina_46: In4in6Addr,
        ina_6: libc::in6_addr,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InV6 {
        in6_hlim: u8,
        in6_cksum: libc::c_int,
        in6_ifindex: libc::c_ushort,
        in6_hops: libc::c_short,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InSockinfo {
        insi_fport: libc::c_int,
        insi_lport: libc::c_int,
        insi_gencnt: u64,
        insi_flags: u32,
        insi_flow: u32,
        insi_vflag: u8,
        insi_ip_ttl: u8,
        rfu_1: u32,
        insi_faddr: InAddr,
        insi_laddr: InAddr,
        insi_v4: u8,
        insi_v6: InV6,
    }

    /// `un_sockinfo`, the union's largest member, which sizes it: two opaque handles and two
    /// `SOCK_MAXADDRLEN` (255) addresses.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct UnSockinfo {
        unsi_conn_so: u64,
        unsi_conn_pcb: u64,
        unsi_addr: [u8; 255],
        unsi_caddr: [u8; 255],
    }

    #[repr(C)]
    union SoiProto {
        pri_in: InSockinfo,
        pri_un: UnSockinfo,
    }

    #[repr(C)]
    struct SocketInfo {
        soi_stat: libc::vinfo_stat,
        soi_so: u64,
        soi_pcb: u64,
        soi_type: libc::c_int,
        soi_protocol: libc::c_int,
        soi_family: libc::c_int,
        soi_options: libc::c_short,
        soi_linger: libc::c_short,
        soi_state: libc::c_short,
        soi_qlen: libc::c_short,
        soi_incqlen: libc::c_short,
        soi_qlimit: libc::c_short,
        soi_timeo: libc::c_short,
        soi_error: libc::c_ushort,
        soi_oobmark: u32,
        soi_rcv: SockbufInfo,
        soi_snd: SockbufInfo,
        soi_kind: libc::c_int,
        rfu_1: u32,
        soi_proto: SoiProto,
    }

    /// PROC_PIDFDSOCKETINFO's reply, `struct socket_fdinfo`.
    #[repr(C)]
    struct SocketFdinfo {
        pfi: ProcFileinfo,
        psi: SocketInfo,
    }

    /// Every process now (proc_listallpids(3)), asked again while its reply fills what it was
    /// given, which processes started meanwhile may have cut short. Each ask has a slot more
    /// than the count said, so that a reply which fills them all says the count grew.
    pub(super) fn processes() -> Vec<libc::pid_t> {
        loop {
            // SAFETY: no buffer: the call says how many processes there are.
            let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
            let count = usize::try_from(count)
                .ok()
                .filter(|count| *count > 0)
                .unwrap_or_else(|| panic!("proc_listallpids: {}", std::io::Error::last_os_error()));
            let mut pids: Vec<libc::pid_t> = vec![0; count.checked_add(1).unwrap()];
            let room = libc::c_int::try_from(size_of_val(pids.as_slice())).unwrap();
            // SAFETY: a buffer of `room` bytes for the call to fill.
            let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), room) };
            let filled = usize::try_from(filled).unwrap();
            if filled < pids.len() {
                pids.truncate(filled);
                return pids;
            }
        }
    }

    /// `pid`'s descriptors (PROC_PIDLISTFDS), asked again while the reply fills what it was
    /// given, as its table may have grown since the call said its size, with a slot more than
    /// that, as [`processes`] asks; `None` where libproc tells nothing of it (another user's
    /// process, or one ended meanwhile).
    fn descriptors(pid: libc::pid_t) -> Option<Vec<libc::proc_fdinfo>> {
        loop {
            // SAFETY: no buffer: the call says how many bytes the process's table needs.
            let room = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
            let room = usize::try_from(room).ok().filter(|room| *room > 0)?;
            let entry = size_of::<libc::proc_fdinfo>();
            let mut fds = Vec::new();
            fds.resize_with(room.div_ceil(entry).checked_add(1).unwrap(), || {
                libc::proc_fdinfo {
                    proc_fd: 0,
                    proc_fdtype: 0,
                }
            });
            let given = libc::c_int::try_from(size_of_val(fds.as_slice())).ok()?;
            // SAFETY: a buffer of `given` bytes for the call to fill.
            let filled =
                unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), given) };
            let filled = usize::try_from(filled).ok().filter(|filled| *filled > 0)? / entry;
            if filled < fds.len() {
                fds.truncate(filled);
                return Some(fds);
            }
        }
    }

    /// `pid`'s IPv4 UDP sockets: each one's local address and generation, with the descriptor
    /// holding it; `None` where libproc tells nothing of its descriptors.
    pub(super) fn udp(pid: libc::pid_t) -> Option<Vec<(SocketAddrV4, u64, i32)>> {
        let size = libc::c_int::try_from(size_of::<SocketFdinfo>()).unwrap();
        let mut found = Vec::new();
        let socket_kind = u32::try_from(libc::PROX_FDTYPE_SOCKET).unwrap();
        for fd in descriptors(pid)?
            .iter()
            .filter(|fd| fd.proc_fdtype == socket_kind)
        {
            // SAFETY: socket_fdinfo is plain C data, for which all zeroes is a value.
            let mut info: SocketFdinfo = unsafe { std::mem::zeroed() };
            // SAFETY: a buffer of `size` bytes for the call to fill.
            let filled = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDSOCKETINFO,
                    (&raw mut info).cast(),
                    size,
                )
            };
            // A descriptor closed since the listing, or a process ended, tells nothing.
            if filled <= 0 {
                continue;
            }
            // The kernel fills its whole struct: one of another size than this one's is laid
            // out otherwise, and none of its fields is read.
            assert_eq!(
                filled, size,
                "socket_fdinfo's size as sys/proc_info.h lays it out"
            );
            let socket = &info.psi;
            if socket.soi_family != libc::AF_INET
                || socket.soi_protocol != libc::IPPROTO_UDP
                || socket.soi_kind != SOCKINFO_IN
            {
                continue;
            }
            // SAFETY: SOCKINFO_IN says the union holds an in_sockinfo.
            let inet = unsafe { socket.soi_proto.pri_in };
            if inet.insi_vflag & INI_IPV4 == 0 {
                continue;
            }
            // SAFETY: an IPv4 PCB's local address is the union's 4-in-6 form; in_pcb.h's port
            // and address in network order.
            let ip = Ipv4Addr::from(u32::from_be(unsafe { inet.insi_laddr.ina_46 }.i46a_addr4.s_addr));
            let port = u16::from_be(u16::try_from(inet.insi_lport).unwrap());
            found.push((SocketAddrV4::new(ip, port), inet.insi_gencnt, fd.proc_fd));
        }
        Some(found)
    }
}

/// An observer's two sockets (crates/remote/src/net.rs, bind_with_limits), each by its own
/// address, whatever other tests of the process open meanwhile: the endpoint's, and its
/// waker's, which every clone of its sender holds again. Told apart by what changed as its
/// sender was cloned, another test's sockets made in that window were taken for its waker
/// (CI's arm64 musl run 43347049202: three such).
#[cfg(unix)]
fn observer_sockets(endpoint: SocketAddr, sender: &Sender<Ask>) -> Vec<Held> {
    let waker = sender.waker_addr().unwrap();
    let bound = |address: SocketAddr| {
        let SocketAddr::V4(at) = address else {
            panic!("an IPv4 observer socket: {address}");
        };
        let found: Vec<_> = udp_sockets()
            .into_iter()
            .filter(|(local, _)| *local == at)
            .map(|(_, id)| id)
            .collect();
        let [id] = found[..] else {
            panic!("one UDP socket at the observer's {at}: {found:?}");
        };
        id
    };
    let (endpoint, waker) = (bound(endpoint), bound(waker));
    let fds = sockets();
    let held = |id: u64| Held { fd: fds[&id][0], id };
    vec![held(endpoint), held(waker)]
}

/// Which of `held` are not yet returned: still behind a descriptor of this process, or
/// still a socket the kernel lists.
#[cfg(unix)]
pub(super) fn not_returned(held: &[Held]) -> Vec<Held> {
    let ours = sockets();
    let listed: Vec<_> = udp_sockets().into_iter().map(|(_, id)| id).collect();
    held.iter()
        .copied()
        .filter(|socket| ours.contains_key(&socket.id) || listed.contains(&socket.id))
        .collect()
}

pub(super) struct Observer {
    pub(super) scope: Scope,
    pub(super) sender: Sender<Ask>,
    pub(super) events: mpsc::Receiver<Event>,
    /// Its driver's QUIC socket and its waker's.
    #[cfg(unix)]
    pub(super) held: Vec<Held>,
    ended: mpsc::Receiver<Result<(), shards_remote::Error>>,
    outcome: OnceLock<Result<(), String>>,
    thread: Option<JoinHandle<()>>,
}

impl Observer {
    pub(super) fn start<'a>(
        owner: &Identity,
        metadata: &Metadata,
        scope: Scope,
        address: SocketAddr,
        until: impl Into<Until<'a>>,
    ) -> Self {
        let window = metadata.capacity.checked_add(managed::DATA_HEAD).unwrap();
        let admitted = Admitted {
            record_bytes: metadata.capacity,
            transport_bytes: u64::try_from(window).unwrap(),
            // RFC 8446 Handshake length: uint24 plus the four-byte handshake header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
        };
        let (event, events) = mpsc::channel();
        let client = Client {
            config: Some(managed::client_config(owner, metadata.server, admitted).unwrap()),
            scope,
            conn: None,
            stream: None,
            events: event,
            pending: Vec::new(),
            #[cfg(test)]
            control_receipts: None,
            inbound: Vec::new(),
            finish: false,
            done: false,
            deadline: Into::<Until<'_>>::into(until).deadline(),
            window,
        };
        let (mut driver, sender) = Driver::bind_bounded(
            "127.0.0.1:0".parse().unwrap(),
            None,
            client,
            DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: admitted.datagram_bytes,
            },
        )
        .unwrap();
        #[cfg(unix)]
        let held = observer_sockets(driver.local().unwrap(), &sender);
        let (finished, ended) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("managed-owner-observer".into())
            .spawn(move || {
                let _ = finished.send(driver.run());
            })
            .unwrap();
        sender.send(Command::Handler(Ask::Connect(address))).unwrap();
        Self {
            scope,
            sender,
            events,
            #[cfg(unix)]
            held,
            ended,
            outcome: OnceLock::new(),
            thread: Some(thread),
        }
    }

    #[track_caller]
    pub(super) fn next<'a>(&self, until: impl Into<Until<'a>>) -> Event {
        match receive(&self.events, until.into()) {
            Ok(event) => event,
            // The handler holds the event sender and drops with the driver once it ends.
            Err(mpsc::RecvTimeoutError::Disconnected) => match self.outcome(Until::At(Instant::now())) {
                Some(Ok(())) => panic!("observer events ended before deadline: driver ended Ok"),
                Some(Err(error)) => panic!("observer events ended before deadline: driver {error}"),
                None => panic!("observer events ended before deadline: driver result not yet sent"),
            },
            Err(error) => panic!("observer event before deadline: {error}"),
        }
    }

    /// How the driver ended, once it has, taken from its thread as `until` holds its wait and
    /// kept.
    #[track_caller]
    fn outcome(&self, until: Until<'_>) -> Option<&Result<(), String>> {
        if self.outcome.get().is_none() {
            let ended = match receive(&self.ended, until) {
                Ok(result) => result.map_err(|e| format!("failed: {e}")),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err("ended without a result: its thread panicked".into())
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return None,
            };
            let _ = self.outcome.set(ended);
        }
        self.outcome.get()
    }

    #[track_caller]
    pub(super) fn exact<'a>(&self, label: &str, expected: &[u8], until: impl Into<Until<'a>>) -> (u64, u64) {
        let until: Until<'_> = until.into();
        let mut bytes = Vec::new();
        let mut wire = 0u64;
        let mut state = StreamState::New;
        while bytes.len() < expected.len() {
            match self.next(until) {
                Event::Record(offset, chunk) => {
                    assert!(!chunk.is_empty(), "empty member record");
                    assert_eq!(offset, wire);
                    wire = wire.checked_add(u64::try_from(chunk.len()).unwrap()).unwrap();
                    let record = Record::decode(&chunk).unwrap();
                    assert_eq!(record.member, self.scope.member);
                    assert_eq!(record.channel, self.scope.channel);
                    state.accept(record).unwrap();
                    match record.event {
                        CaptureEvent::Open(actual) => assert_eq!(actual, label),
                        CaptureEvent::Data(payload) => {
                            bytes.extend_from_slice(payload);
                            assert!(
                                expected.starts_with(&bytes),
                                "member bytes differ at offset {offset}; lengths actual={} expected={}; controlled_fixture_bytes={bytes:02x?}",
                                bytes.len(),
                                expected.len()
                            );
                            if bytes.len() == expected.len() {
                                return (offset, wire);
                            }
                        }
                        other => panic!("unexpected capture event {other:?}"),
                    }
                    self.sender.send(Command::Handler(Ask::Ack(wire))).unwrap();
                }
                Event::Failed(error) => panic!("observer failed: {error}"),
                other => panic!("expected member record, got {other:?}"),
            }
        }
        panic!("nonempty fixture output required");
    }

    #[track_caller]
    pub(super) fn refused<'a>(&self, expected: Option<Reason>, until: impl Into<Until<'a>>) {
        match self.next(until) {
            Event::Lost(code) => assert_eq!(code, expected.map(|reason| u64::from(reason as u32))),
            Event::Record(offset, bytes) => {
                panic!("refused subscription received {} bytes at {offset}", bytes.len())
            }
            Event::Failed(error) => panic!("observer failed instead of peer refusal: {error}"),
            Event::End => panic!("refused subscription unexpectedly ended normally"),
        }
    }

    #[track_caller]
    pub(super) fn ack_and_finish<'a>(&self, wire: u64, until: impl Into<Until<'a>>) {
        self.sender
            .send(Command::Handler(Ask::AckAndFinish(wire)))
            .unwrap();
        // QUIC FIN follows the consumed frame in the same ordered stream. The service
        // processes controls before its End branch emits this typed close reason.
        self.refused(Some(Reason::Closed), until);
    }

    #[track_caller]
    pub(super) fn join<'a>(&mut self, until: impl Into<Until<'a>>) -> Result<(), String> {
        if self.thread.is_none() {
            return Ok(());
        }
        let Some(result) = self.outcome(until.into()).cloned() else {
            return Err("observer completion deadline".into());
        };
        let joined = self
            .thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| "observer worker panicked".to_string());
        joined.and(result)
    }
}

#[derive(Debug)]
pub(super) struct Metadata {
    pub(super) server: Fingerprint,
    pub(super) owner: Fingerprint,
    pub(super) run: String,
    pub(super) generation: u64,
    pub(super) capacity: usize,
    pub(super) endpoints: BTreeMap<(u32, u8), SocketAddr>,
}

fn fingerprint(text: &str) -> Option<Fingerprint> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (pair, byte) in text.as_bytes().as_chunks::<2>().0.iter().zip(&mut out) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

impl Metadata {
    pub(super) fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != "shards managed output 1" {
            return None;
        }
        let field = |line: &str, name| {
            line.strip_prefix(name)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let server = fingerprint(&field(lines.next()?, "server ")?)?;
        let owner = fingerprint(&field(lines.next()?, "owner ")?)?;
        let run = field(lines.next()?, "run ")?;
        let generation = field(lines.next()?, "generation ")?.parse().ok()?;
        let capacity = field(lines.next()?, "capacity ")?.parse().ok()?;
        if capacity == 0 {
            return None;
        }
        let mut endpoints = BTreeMap::new();
        // What the daemon appends to the record as its channels end, after the endpoints it
        // published (daemon/managed.rs): an output's `canonical` and `state` lines, an
        // input's `state` line, each of an endpoint published. A member that ends at once
        // has its input's end there before its owner reads the record (HVF at 4a426c367).
        let mut ended = false;
        for line in lines {
            let fields: Vec<_> = line.split_whitespace().collect();
            let ["member", member, "channel", channel, word, rest @ ..] = fields.as_slice() else {
                return None;
            };
            let member: u32 = member.parse().ok()?;
            let channel: u8 = channel.parse().ok()?;
            // 1 and 2 a member's output channels, 0 its input (plan step 5).
            if !matches!(channel, 0..=2) {
                return None;
            }
            match (*word, rest) {
                ("address", [address]) if !ended => {
                    let address: SocketAddr = address.parse().ok()?;
                    if !address.ip().is_loopback() || endpoints.insert((member, channel), address).is_some() {
                        return None;
                    }
                }
                ("canonical" | "state", [_, ..]) if endpoints.contains_key(&(member, channel)) => {
                    ended = true
                }
                _ => return None,
            }
        }
        Some(Self {
            server,
            owner,
            run,
            generation,
            capacity,
            endpoints,
        })
    }

    pub(super) fn scope(&self, member: u32, channel: Channel) -> Scope {
        Scope {
            run: RunId(Sha256::digest(self.run.as_bytes()).into()),
            generation: self.generation,
            member,
            channel,
        }
    }
}

/// What an owner writing a member's input heard of its endpoint.
#[derive(Debug)]
pub(super) enum InputHeard {
    Said(managed::input::Said),
    End,
    Lost(Option<u64>),
    Failed(String),
}

pub(super) enum InputAsk {
    Connect(SocketAddr),
    Send(Vec<u8>),
    /// Closes its connection, as an owner that goes does: its member's input stays open.
    Close,
}

/// An owner of a member's input, as an orchestrator's adapter is: it subscribes, then sends
/// what it is asked to, and tells what the endpoint says.
struct InputClient {
    config: Option<ClientConfig>,
    scope: managed::input::Scope,
    offset: u64,
    stream: Option<(ConnKey, StreamId)>,
    pending: Vec<u8>,
    inbound: Vec<u8>,
    heard: mpsc::Sender<InputHeard>,
    deadline: Option<Instant>,
    done: bool,
}

impl InputClient {
    fn flush(&mut self, io: &mut Io<'_>) {
        let Some((conn, stream)) = self.stream else {
            return;
        };
        while !self.pending.is_empty() {
            match io.write(conn, stream, &self.pending) {
                Ok(0) => return,
                Ok(n) => drop(self.pending.drain(..n)),
                Err(error) => {
                    let _ = self.heard.send(InputHeard::Failed(error.to_string()));
                    self.done = true;
                    return;
                }
            }
        }
    }
}

impl Handler for InputClient {
    type Command = InputAsk;
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> Result<(), Close> {
        let stream = io.open(conn).map_err(|_| Close {
            code: 1,
            why: "fixture input stream failed",
        })?;
        self.stream = Some((conn, stream));
        let mut first = managed::input::subscribe(self.scope, self.offset).to_vec();
        first.append(&mut self.pending);
        self.pending = first;
        self.flush(io);
        Ok(())
    }
    fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let (bytes, ended) = match io.read(conn, stream) {
                Read::Bytes(bytes) => (bytes, false),
                Read::End(bytes) => (bytes, true),
                Read::Blocked => return,
                Read::Reset => {
                    let _ = self.heard.send(InputHeard::Failed("input stream reset".into()));
                    self.done = true;
                    return;
                }
            };
            self.inbound.extend_from_slice(&bytes);
            loop {
                match managed::input::said(&self.inbound) {
                    Ok(Some((said, n))) => {
                        let _ = self.heard.send(InputHeard::Said(said));
                        self.inbound.drain(..n);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = self.heard.send(InputHeard::Failed(error.to_string()));
                        self.done = true;
                        return;
                    }
                }
            }
            if ended {
                let _ = self.heard.send(InputHeard::End);
                return;
            }
        }
    }
    fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {
        let _ = self.heard.send(InputHeard::Failed("input stream stopped".into()));
        self.done = true;
    }
    fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, _: &str, code: Option<u64>) {
        let _ = self.heard.send(InputHeard::Lost(code));
        self.done = true;
    }
    fn command(&mut self, io: &mut Io<'_>, ask: InputAsk) {
        match ask {
            InputAsk::Connect(address) => {
                if let Some(Err(error)) = self.config.take().map(|config| io.connect(address, config)) {
                    let _ = self.heard.send(InputHeard::Failed(error.to_string()));
                    self.done = true;
                }
            }
            InputAsk::Send(bytes) => {
                self.pending.extend_from_slice(&bytes);
                self.flush(io);
            }
            InputAsk::Close => {
                if let Some((conn, _)) = self.stream {
                    io.close(conn, 0, "fixture input owner gone");
                }
                self.done = true;
            }
        }
    }
    fn done(&self) -> bool {
        self.done
    }
    fn due(&self) -> Option<Instant> {
        self.deadline
    }
    fn timeout(&mut self, _: &mut Io<'_>, _: Instant) {
        let _ = self
            .heard
            .send(InputHeard::Failed("input writer deadline".into()));
        self.done = true;
    }
}

/// A member's input, written by its owner over its endpoint: one record in flight, each
/// sent once the endpoint says the last was taken.
pub(super) struct InputWriter {
    pub(super) sender: Sender<InputAsk>,
    pub(super) heard: mpsc::Receiver<InputHeard>,
    thread: Option<JoinHandle<Result<(), shards_remote::Error>>>,
    /// Where the next record goes and how long it may be, as last said.
    pub(super) at: u64,
    room: usize,
}

impl InputWriter {
    /// Subscribes to `member`'s input at `address`, from offset `offset`, and waits to be
    /// told where its first record goes.
    #[track_caller]
    pub(super) fn start<'a>(
        owner: &Identity,
        metadata: &Metadata,
        member: u32,
        address: SocketAddr,
        offset: u64,
        until: impl Into<Until<'a>>,
    ) -> Result<Self, InputHeard> {
        let until: Until<'_> = until.into();
        let admitted = Admitted {
            record_bytes: shards_abi::run::CHUNK,
            transport_bytes: u64::try_from(managed::input::TAKEN_BYTES).unwrap(),
            // RFC 8446 Handshake length: uint24 plus the four-byte handshake header.
            handshake_bytes: (1 << 24) - 1 + 4,
            datagram_bytes: managed::DATAGRAM_BYTES,
        };
        let (heard, hearing) = mpsc::channel();
        let client = InputClient {
            config: Some(managed::input::client_config(owner, metadata.server, admitted).unwrap()),
            scope: managed::input::Scope {
                run: RunId(Sha256::digest(metadata.run.as_bytes()).into()),
                generation: metadata.generation,
                member,
            },
            offset,
            stream: None,
            pending: Vec::new(),
            inbound: Vec::new(),
            heard,
            deadline: until.deadline(),
            done: false,
        };
        let (mut driver, sender) = Driver::bind_bounded(
            "127.0.0.1:0".parse().unwrap(),
            None,
            client,
            DriverLimits {
                connections: 1,
                command_slots: 4,
                queued_datagram_bytes: admitted.datagram_bytes,
            },
        )
        .unwrap();
        let thread = std::thread::Builder::new()
            .name("fixture-input".into())
            .spawn(move || driver.run())
            .unwrap();
        sender.send(Command::Handler(InputAsk::Connect(address))).unwrap();
        let mut writer = Self {
            sender,
            heard: hearing,
            thread: Some(thread),
            at: offset,
            room: 0,
        };
        writer.taken(until)?;
        Ok(writer)
    }

    /// What its endpoint says next, `awaited` within `until`. A driver that ends first, its
    /// client and the channel with it, is joined and how it ended told, its thread's own
    /// panic or failure, never a wait left to the deadline.
    #[track_caller]
    pub(super) fn next<'a>(
        &mut self,
        until: impl Into<Until<'a>>,
        awaited: &str,
    ) -> Result<InputHeard, InputHeard> {
        match receive(&self.heard, until.into()) {
            Ok(heard) => Ok(heard),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(InputHeard::Failed(format!(
                "no {awaited} within the fixture's patience"
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(InputHeard::Failed(
                match self.thread.take().map(JoinHandle::join) {
                    Some(Ok(Ok(()))) => format!("no {awaited}: its driver ended Ok"),
                    Some(Ok(Err(error))) => format!("no {awaited}: its driver failed: {error}"),
                    Some(Err(_)) => format!("no {awaited}: its driver's thread panicked"),
                    None => format!("no {awaited}: its driver had ended"),
                },
            )),
        }
    }

    /// Waits for the endpoint to say where the next record goes.
    #[track_caller]
    fn taken(&mut self, until: Until<'_>) -> Result<u64, InputHeard> {
        match self.next(until, "taken")? {
            InputHeard::Said(managed::input::Said::Taken { accepted, room }) => {
                self.at = accepted;
                self.room = room as usize;
                Ok(accepted)
            }
            other => Err(other),
        }
    }

    /// Writes `bytes` to the member's stdin, a record at a time, each within the room it was
    /// told, waiting for each to be taken; the offset after them.
    #[track_caller]
    pub(super) fn write_all<'a>(
        &mut self,
        bytes: &[u8],
        until: impl Into<Until<'a>>,
    ) -> Result<u64, InputHeard> {
        let until: Until<'_> = until.into();
        for part in bytes.chunks(self.room.max(1)) {
            let mut frame = Vec::new();
            managed::input::record(self.at, part, &mut frame).unwrap();
            self.sender.send(Command::Handler(InputAsk::Send(frame))).unwrap();
            let before = self.at;
            let at = self.taken(until)?;
            // A record is taken whole, at its offset.
            if Some(at) != before.checked_add(part.len() as u64) {
                return Err(InputHeard::Failed(format!(
                    "a record of {} bytes at {before} taken to {at}",
                    part.len()
                )));
            }
        }
        Ok(self.at)
    }

    /// Ends the member's stdin (`errno` 0) or aborts it, and waits for its end.
    #[track_caller]
    pub(super) fn end<'a>(
        &mut self,
        errno: i32,
        until: impl Into<Until<'a>>,
    ) -> Result<(u64, InputEnd), InputHeard> {
        self.sender
            .send(Command::Handler(InputAsk::Send(
                managed::input::end(self.at, errno).to_vec(),
            )))
            .unwrap();
        match self.next(until, "end")? {
            InputHeard::Said(managed::input::Said::Ended { written, end }) => Ok((written, end)),
            other => Err(other),
        }
    }
}

impl Drop for InputWriter {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Handler(InputAsk::Close));
        self.sender.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

//! virtio-fs (virtio 1.3 §5.11): a host directory the guest mounts by its tag, its FUSE
//! requests answered by [`server::Server`] in a process of its own (the share process,
//! D38), reached over a Unix socket: the VM process, confined to its own files (D30),
//! reaches no directory it is given after it starts. Two queues, as Linux's driver takes
//! one high priority queue and the request queues the device says it has (here one):
//! FORGETs and INTERRUPTs come on the first, everything else on the second.

pub mod host;
pub mod server;

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use super::limit::Limiter;
use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, ShmRegion, VirtioDevice, feature};
use crate::debug;
use crate::memory::{Access, GuestMemory};
use crate::snapshot::codec::{self, Reader, Writer};

pub const DEVICE_ID: u32 = 26;
const QUEUE_SIZE: u16 = 256;
/// The most a tag holds (virtio 1.3 §5.11.4).
pub const TAG_BYTES: usize = 36;

/// Where a share's server is reached once its directory is known: a VM restored ahead of
/// its run has its devices, and learns its directories with the run (D38).
#[derive(Debug, Default)]
pub struct Slot {
    conn: std::sync::Mutex<Option<UnixStream>>,
    /// The tables of the server a restored device's guest was served by (D176), for the
    /// server it is given next.
    pending: std::sync::Mutex<Option<Vec<u8>>>,
}

/// A share's slot, which the VM's owner fills.
pub type Share = Arc<Slot>;

/// The slot a device asks through: a run's share's, or the join share's, a `static` of the
/// VM process's (D119): one VM a process, so one join share, its device and the process's
/// joiners reaching the same.
#[derive(Clone, Debug)]
pub enum SlotOf {
    Run(Share),
    Join(&'static Slot),
}

impl std::ops::Deref for SlotOf {
    type Target = Slot;
    fn deref(&self) -> &Slot {
        match self {
            SlotOf::Run(s) => s,
            SlotOf::Join(s) => s,
        }
    }
}

/// The most a request or reply frame holds: a write's data and its headers.
pub const MAX_FRAME: usize = server::MAX_WRITE as usize + 4096;

/// A frame of the VM process's own, never of a guest's (D176): its length word's top bit,
/// which no guest's request (at most [`MAX_FRAME`]) sets, as the device sends what the
/// guest asks. Its first byte names what it asks, and its answer is one too.
const CONTROL: u32 = 1 << 31;
/// A control frame's ask: the server's tables, or that it take them.
const SAVE: u8 = 1;
const RESTORE: u8 = 2;
/// The most a control frame holds: a share's tables, which a snapshot's state holds.
const MAX_CONTROL: usize = server::MAX_TABLES;

impl Slot {
    /// A slot no server is in yet.
    pub const fn new() -> Slot {
        Slot {
            conn: std::sync::Mutex::new(None),
            pending: std::sync::Mutex::new(None),
        }
    }

    /// Asks the server at the other end of `conn` from here on, which first takes the tables
    /// a restored device brought (D176): Err where it does not, its guest's every file of
    /// the share then unknown to it.
    pub fn attach(&self, mut conn: UnixStream) -> Result<(), String> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(tables) = pending {
            control(&mut conn, RESTORE, &tables).map_err(|e| format!("its share's server: {e}"))?;
        }
        *self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(conn);
        Ok(())
    }

    /// Its server's tables (D176), none with no server: asked while its device is paused,
    /// so between requests.
    fn tables(&self) -> Result<Option<Vec<u8>>, String> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match conn.as_mut() {
            Some(conn) => control(conn, SAVE, &[]).map(Some),
            None => Ok(None),
        }
    }

    /// The server's reply to `req`, none for one that takes none; Err with no server, or
    /// with one that has gone. One request at a time: each queue's worker asks in turn.
    fn ask(&self, req: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = conn.as_mut().ok_or(io::ErrorKind::NotConnected)?;
        write_frame(conn, req)?;
        let reply = read_frame(conn)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        Ok((!reply.is_empty()).then_some(reply))
    }
}

/// Sends `bytes` as a frame: its length (le32), then itself.
pub fn write_frame(to: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(bytes);
    to.write_all(&frame)
}

/// The next frame, none at the end; Err for one past [`MAX_FRAME`].
pub fn read_frame(from: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match from.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {len}-byte frame"),
        ));
    }
    let mut bytes = vec![0u8; len];
    from.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}

/// The bytes of the host's storage a request reads or writes, which its microVM's I/O
/// limits charge (D178): a READ's asked for, a WRITE's given; none of anything else it asks.
fn io_bytes(req: &[u8]) -> u64 {
    let word = |at: usize| {
        req.get(at..at.saturating_add(4))
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    match word(4) {
        // fuse_read_in: fh, offset, then size.
        Some(server::op::READ) => word(server::HEADER_BYTES + 16).map_or(0, u64::from),
        // fuse_write_in's 40 bytes, then the data.
        Some(server::op::WRITE) => {
            u64::try_from(req.len().saturating_sub(server::HEADER_BYTES + 40)).unwrap_or(u64::MAX)
        }
        _ => 0,
    }
}

/// Sends `ask` and `bytes` as a control frame, and reads its answer: what it brings, or the
/// server's why not.
fn control(conn: &mut UnixStream, ask: u8, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut frame = Vec::with_capacity(1 + bytes.len());
    frame.push(ask);
    frame.extend_from_slice(bytes);
    write_control(conn, &frame).map_err(|e| e.to_string())?;
    match read_any(conn).map_err(|e| e.to_string())? {
        Some(Frame::Control(answer)) => match answer.split_first() {
            Some((0, brought)) => Ok(brought.to_vec()),
            Some((_, why)) => Err(String::from_utf8_lossy(why).into_owned()),
            None => Err("an empty answer".into()),
        },
        Some(Frame::Request(_)) => Err("an answer that is no control frame's".into()),
        None => Err("it closed first".into()),
    }
}

fn write_control(to: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_CONTROL {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let len = u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)? | CONTROL;
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(bytes);
    to.write_all(&frame)
}

/// A frame as the share process reads it: a guest's request, or the VM process's own.
enum Frame {
    Request(Vec<u8>),
    Control(Vec<u8>),
}

/// The next frame, none at the end; Err for one past its kind's most.
fn read_any(from: &mut UnixStream) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match from.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let word = u32::from_le_bytes(len);
    let (control, len, most) = if word & CONTROL != 0 {
        (true, (word & !CONTROL) as usize, MAX_CONTROL)
    } else {
        (false, word as usize, MAX_FRAME)
    };
    if len > most {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {len}-byte frame"),
        ));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|e| io::Error::other(format!("a {len}-byte frame: {e}")))?;
    bytes.resize(len, 0);
    from.read_exact(&mut bytes)?;
    Ok(Some(if control {
        Frame::Control(bytes)
    } else {
        Frame::Request(bytes)
    }))
}

/// Answers `conn`'s requests with `server` until it closes: the share process's side.
/// A request that takes no reply gets an empty frame, so that the asker waits on none. A
/// control frame, the VM process's own, asks for the server's tables, or gives them
/// (D176), and is answered with one: 0 then what it asked for, or 1 then why not.
pub fn answer(server: &server::Server, mut conn: UnixStream) -> io::Result<()> {
    while let Some(frame) = read_any(&mut conn)? {
        match frame {
            Frame::Request(req) => {
                let reply = server.handle(&req).unwrap_or_default();
                write_frame(&mut conn, &reply)?;
            }
            Frame::Control(asked) => {
                let done = match asked.split_first() {
                    Some((&SAVE, [])) => server.save(),
                    Some((&RESTORE, tables)) => server.restore(tables).map(|()| Vec::new()),
                    _ => Err("a control frame of no known ask".into()),
                };
                let answer = match done {
                    Ok(brought) => [&[0u8][..], &brought].concat(),
                    Err(why) => [&[1u8][..], why.as_bytes()].concat(),
                };
                write_control(&mut conn, &answer)?;
            }
        }
    }
    Ok(())
}

/// A shared directory: its tag, and the slot its server is in.
pub struct Fs {
    tag: [u8; TAG_BYTES],
    server: SlotOf,
    context: Option<(Arc<GuestMemory>, Arc<DeviceInterrupt>)>,
    workers: Vec<Worker>,
    paused: Vec<Queue>,
    /// Its server's tables as its last [`unsaved`](VirtioDevice::unsaved) found them, for
    /// the save that follows it (D176).
    tables: std::sync::Mutex<Option<Vec<u8>>>,
    /// Its microVM's I/O limits (D178), and whether this device holds them paused.
    limit: Option<&'static Limiter>,
    holds_limit: bool,
    /// Its DAX window (D189): the cache, shared memory region 0 (VIRTIO_FS_SHMCAP_ID_CACHE),
    /// which its guest maps the files it reads into; none where it has none.
    window: Option<[ShmRegion; 1]>,
}

/// The cache's shared memory region's id (include/uapi/linux/virtio_fs.h).
pub const CACHE: u8 = 0;

impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs").finish_non_exhaustive()
    }
}

impl Fs {
    /// The device of the server in `slot`, mounted by `tag`.
    pub fn new(tag: &str, slot: Share) -> Result<Fs, String> {
        Fs::of(tag, SlotOf::Run(slot))
    }

    /// The join share's device (D119): the server in `slot`, which its VM's joiners' volumes
    /// are given to, mounted by `tag`.
    pub fn join(tag: &str, slot: &'static Slot) -> Result<Fs, String> {
        Fs::of(tag, SlotOf::Join(slot))
    }

    fn of(tag: &str, slot: SlotOf) -> Result<Fs, String> {
        if tag.is_empty() || tag.len() > TAG_BYTES {
            return Err(format!("virtio-fs tag {tag:?}: 1 to {TAG_BYTES} bytes"));
        }
        let mut bytes = [0u8; TAG_BYTES];
        for (d, s) in bytes.iter_mut().zip(tag.bytes()) {
            *d = s;
        }
        Ok(Fs {
            tag: bytes,
            server: slot,
            context: None,
            workers: Vec::new(),
            paused: Vec::new(),
            tables: std::sync::Mutex::new(None),
            limit: None,
            holds_limit: false,
            window: None,
        })
    }

    /// Its requests charged to its microVM's I/O limits (D178): its request queue's, the
    /// high priority queue's FORGETs and INTERRUPTs none.
    pub fn with_limit(mut self, limit: Option<&'static Limiter>) -> Fs {
        self.limit = limit;
        self
    }

    /// The device with a DAX window of `len` bytes at guest address `base` (D189), where
    /// `window` gives one.
    pub fn with_window(mut self, window: Option<(u64, u64)>) -> Fs {
        self.window = window.map(|(base, len)| [ShmRegion { id: CACHE, base, len }]);
        self
    }

    fn start(&mut self, queues: Vec<Queue>) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = queues;
            return Err("virtio-fs started before activation".into());
        };
        let mut queues = queues.into_iter();
        while let Some(queue) = queues.next() {
            let (server, mem) = (self.server.clone(), memory.clone());
            // The first queue is the high priority one (FORGET, INTERRUPT): no I/O.
            let limit = self.limit.filter(|_| !self.workers.is_empty());
            let answer = move |chain: &Chain| serve(chain, &mem, &server, limit);
            match Worker::start("virtio-fs", queue, memory.clone(), interrupt.clone(), answer) {
                Ok(w) => self.workers.push(w),
                Err((e, queue)) => {
                    let mut stopped: Vec<Queue> = self.workers.drain(..).filter_map(Worker::stop).collect();
                    stopped.push(queue);
                    stopped.extend(queues);
                    self.paused = stopped;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Vec<Queue> {
        self.workers.drain(..).filter_map(Worker::stop).collect()
    }

    /// Lets go of its microVM's limits, where its pause holds them.
    fn release_limit(&mut self) {
        if let Some(limit) = self.limit.filter(|_| self.holds_limit) {
            limit.resume();
            self.holds_limit = false;
        }
    }
}

/// Answers one request: its readable bytes to the server, its reply into the writable
/// descriptors. The bytes written. Guest memory is held to read the request and to write
/// the reply, never while the share answers (D29): a share slow to answer, or never
/// answering, held every other device's queue work, and a snapshot, off guest memory for
/// as long (audit V04).
fn serve(chain: &Chain, mem: &GuestMemory, slot: &Slot, limit: Option<&Limiter>) -> u32 {
    // Every FUSE request fits a frame, the largest write with its headers: a chain that
    // claims more, up to 256 descriptors of 4 GiB, is answered EINVAL from its header,
    // with nothing allocated for the rest (audit V03).
    let total = chain
        .readable()
        .try_fold(0usize, |n, d| n.checked_add(d.len as usize))
        .filter(|&n| n <= MAX_FRAME);
    let mut req = vec![0u8; total.unwrap_or(server::HEADER_BYTES)];
    if !mem.access().is_ok_and(|a| gather(chain, &a, &mut req)) {
        debug!("virtio-fs: a request outside guest memory");
        return 0;
    }
    // Charged to its microVM's I/O limits before it is asked (D178).
    if let (Some(limit), Some(_)) = (limit, total) {
        limit.take(io_bytes(&req));
    }
    let reply = match total {
        // No directory, or its server gone: the guest is told so (ENODEV), as for a share
        // it may not mount.
        Some(_) => slot.ask(&req).unwrap_or_else(|_| server::unattached(&req)),
        None => server::too_long(&req),
    };
    let Some(reply) = reply else {
        return 0;
    };
    let Ok(access) = mem.access() else {
        return 0;
    };
    let mut rest = reply.as_slice();
    let mut written = 0u32;
    for d in chain.writable() {
        if rest.is_empty() {
            break;
        }
        let n = rest.len().min(d.len as usize);
        let (chunk, tail) = rest.split_at(n);
        if access.write(d.addr, chunk).is_err() {
            return written;
        }
        written = written.saturating_add(n as u32);
        rest = tail;
    }
    written
}

/// Fills `buf` with the chain's readable bytes, in order, as far as they go; false if one
/// it needs lies outside guest memory.
fn gather(chain: &Chain, mem: &Access<'_>, buf: &mut [u8]) -> bool {
    let mut at = 0;
    for d in chain.readable() {
        let Some(dst) = buf.get_mut(at..).filter(|rest| !rest.is_empty()) else {
            break;
        };
        let n = dst.len().min(d.len as usize);
        if dst.get_mut(..n).is_none_or(|dst| mem.read(d.addr, dst).is_err()) {
            return false;
        }
        at += n;
    }
    true
}

impl VirtioDevice for Fs {
    fn shm_regions(&self) -> &[ShmRegion] {
        self.window.as_ref().map_or(&[], |w| w.as_slice())
    }

    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        feature::VERSION_1 | feature::EVENT_IDX | feature::INDIRECT_DESC
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_fs_config: tag[36], num_request_queues (le32).
        let mut config = [0u8; TAG_BYTES + 4];
        config[..TAG_BYTES].copy_from_slice(&self.tag);
        config[TAG_BYTES..].copy_from_slice(&1u32.to_le_bytes());
        let start = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = start
                .checked_add(i)
                .and_then(|at| config.get(at))
                .copied()
                .unwrap_or(0);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}

    fn activate(&mut self, activation: Activation) -> Result<(), String> {
        let Activation {
            memory,
            queues,
            interrupt,
            ..
        } = activation;
        self.context = Some((memory, interrupt));
        self.start(queues).inspect_err(|_| self.paused.clear())
    }

    fn notify(&self, queue: u16) {
        if let Some(w) = self.workers.get(usize::from(queue)) {
            w.notify();
        }
    }

    fn reset(&mut self) {
        self.stop();
        self.paused.clear();
        self.context = None;
        self.release_limit();
    }

    /// Its limits paused first (D178): a worker waiting on them answers the request in hand
    /// at once, and stops after it.
    fn pause(&mut self) -> Vec<super::QueueState> {
        if let Some(limit) = self.limit.filter(|_| !self.holds_limit) {
            limit.pause();
            self.holds_limit = true;
        }
        let stopped = self.stop();
        if !stopped.is_empty() {
            self.paused = stopped;
        }
        self.paused.iter().map(Queue::state).collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        self.release_limit();
        let queues = std::mem::take(&mut self.paused);
        if queues.is_empty() {
            Ok(())
        } else {
            self.start(queues)
        }
    }

    /// A served share's files are its server's, which the guest holds by node IDs that
    /// live in the server alone (D176): its server's tables are asked for here, as the
    /// snapshot's devices are paused, for the save that follows to keep, and what it
    /// cannot keep refuses the snapshot.
    fn unsaved(&self) -> Option<String> {
        match self.server.tables() {
            Ok(tables) => {
                *self
                    .tables
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = tables;
                None
            }
            Err(e) => Some(format!("a volume's server: {e}")),
        }
    }

    /// Its server's tables, which a restore gives the server its run is given: none where
    /// it had none, a template's.
    fn save(&self, w: &mut Writer) {
        let tables = self
            .tables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        w.bytes(tables.as_deref().unwrap_or_default());
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> codec::Result<()> {
        let tables = r.bytes(MAX_CONTROL)?;
        if !tables.is_empty() {
            *self
                .server
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tables.to_vec());
        }
        Ok(())
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::super::queue::Descriptor;
    use super::*;

    const BASE: u64 = 0x8000_0000;
    const RAM: usize = 4 << 20;
    const REQUEST: u64 = BASE;
    const REPLY: u64 = BASE + 0x1000;

    /// A FUSE_GETATTR of the root, `unique` 0x1234, at `REQUEST`.
    fn guest() -> GuestMemory {
        let mem = GuestMemory::anonymous(&[(BASE, RAM)]).unwrap();
        let mut req = Vec::new();
        for v in [56u32, 3] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(&0x1234u64.to_le_bytes());
        req.extend_from_slice(&1u64.to_le_bytes());
        req.extend_from_slice(&[0u8; 32]);
        mem.access().unwrap().write(REQUEST, &req).unwrap();
        mem
    }

    fn readable(addr: u64, len: u32) -> Descriptor {
        Descriptor {
            addr,
            len,
            writable: false,
        }
    }

    /// `readables`, then a writable buffer of a page at `REPLY`.
    fn chain(readables: Vec<Descriptor>) -> Chain {
        let mut descriptors = readables;
        descriptors.push(Descriptor {
            addr: REPLY,
            len: 4096,
            writable: true,
        });
        Chain { head: 0, descriptors }
    }

    /// The reply's (len, error, unique).
    fn reply(mem: &GuestMemory) -> (u32, i32, u64) {
        let mut out = [0u8; 16];
        mem.access().unwrap().read(REPLY, &mut out).unwrap();
        mem.access().unwrap().write(REPLY, &[0u8; 16]).unwrap();
        (
            u32::from_le_bytes(out[0..4].try_into().unwrap()),
            i32::from_le_bytes(out[4..8].try_into().unwrap()),
            u64::from_le_bytes(out[8..16].try_into().unwrap()),
        )
    }

    /// A share that answers each request with an empty success, after `hold` lets it.
    /// A share's tables go by control frames, the VM process's own, between a slot and its
    /// share process's [`answer`] (D176): a restored device's slot gives its new server what
    /// its old one said, before it asks it anything, and a guest's request, however it is
    /// made, is never one. A server that refuses them leaves the slot with no server.
    #[test]
    fn a_shares_tables_go_to_the_server_its_restored_slot_is_given() {
        let dir = shards_testdir::TempDir::new("fs-tables").unwrap();
        std::fs::write(dir.join("f"), "f").unwrap();
        let serve = || {
            let server = server::Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
            let (ours, theirs) = UnixStream::pair().unwrap();
            let t = std::thread::spawn(move || answer(&server, theirs).unwrap());
            (ours, t)
        };
        let (conn, first) = serve();
        let slot = Slot::new();
        slot.attach(conn).unwrap();
        // A LOOKUP, as a guest's request goes, then the server's tables.
        let mut lookup = Vec::new();
        lookup.extend((40u32 + 2).to_le_bytes());
        lookup.extend(1u32.to_le_bytes());
        lookup.extend(7u64.to_le_bytes());
        lookup.extend(1u64.to_le_bytes());
        lookup.extend([0u8; 16]);
        lookup.extend(b"f\0");
        let reply = slot.ask(&lookup).unwrap().unwrap();
        assert_eq!(i32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        let tables = slot.tables().unwrap().unwrap();
        assert!(!tables.is_empty());
        drop(slot);
        first.join().unwrap();
        // Restored: its next server takes them as it is given.
        let restored = Slot::new();
        *restored.pending.lock().unwrap() = Some(tables.clone());
        let (conn, second) = serve();
        restored.attach(conn).unwrap();
        let reply = restored.ask(&lookup).unwrap().unwrap();
        assert_eq!(i32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        drop(restored);
        second.join().unwrap();
        // Tables a server cannot take: the slot is given no server.
        let refused = Slot::new();
        *refused.pending.lock().unwrap() = Some(tables[..tables.len() - 1].to_vec());
        let (conn, third) = serve();
        assert!(refused.attach(conn).unwrap_err().contains("its share's server"));
        assert!(refused.ask(&lookup).is_err());
        third.join().unwrap();
        // A guest's request is a frame of at most MAX_FRAME, its length word's top bit clear.
        assert!(MAX_FRAME < CONTROL as usize);
    }

    fn share(hold: mpsc::Receiver<()>, asked: mpsc::Sender<Vec<u8>>) -> (Share, std::thread::JoinHandle<()>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let slot = Share::default();
        slot.attach(ours).unwrap();
        let server = std::thread::spawn(move || {
            let mut conn = theirs;
            while let Ok(Some(req)) = read_frame(&mut conn) {
                let _ = asked.send(req.clone());
                if hold.recv().is_err() {
                    return;
                }
                let mut out = 16u32.to_le_bytes().to_vec();
                out.extend_from_slice(&0i32.to_le_bytes());
                out.extend_from_slice(&req[8..16]);
                if write_frame(&mut conn, &out).is_err() {
                    return;
                }
            }
        });
        (slot, server)
    }

    /// A chain whose readable descriptors claim more than any FUSE request holds is
    /// answered EINVAL from its header alone: nothing is allocated for what it claims (256
    /// descriptors of 4 GiB, a TiB), and nothing reaches the share, which goes on
    /// answering.
    #[test]
    fn a_request_longer_than_any_is_refused_unread() {
        let mem = guest();
        let (go, hold) = mpsc::channel();
        let (asked, requests) = mpsc::channel();
        let (slot, server) = share(hold, asked);
        let outside = BASE + RAM as u64;
        let mut claims = vec![readable(REQUEST, 56)];
        claims.extend((0..255).map(|_| readable(outside, u32::MAX)));
        let just_over = u32::try_from(MAX_FRAME + 1 - 56).unwrap();
        for readables in [
            claims,
            vec![readable(REQUEST, 56), readable(BASE + 0x2000, just_over)],
        ] {
            assert_eq!(serve(&chain(readables), &mem, &slot, None), 16);
            assert_eq!(reply(&mem), (16, -22, 0x1234));
            assert!(requests.try_recv().is_err(), "the share was asked");
        }
        go.send(()).unwrap();
        assert_eq!(serve(&chain(vec![readable(REQUEST, 56)]), &mem, &slot, None), 16);
        assert_eq!(reply(&mem), (16, 0, 0x1234));
        assert_eq!(requests.recv().unwrap().len(), 56);
        drop((slot, go));
        server.join().unwrap();
    }

    /// Guest memory is not held while the share answers (D29): another device, or a
    /// snapshot, takes it meanwhile, rather than waiting on the share for as long as it
    /// takes, or for good.
    #[test]
    fn guest_memory_is_free_while_the_share_answers() {
        let mem = guest();
        let (go, hold) = mpsc::channel();
        let (asked, requests) = mpsc::channel();
        let (slot, server) = share(hold, asked);
        let free = std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve(&chain(vec![readable(REQUEST, 56)]), &mem, &slot, None));
            requests.recv().unwrap();
            let (took, taken) = mpsc::channel();
            let mem = &mem;
            scope.spawn(move || {
                let _held = mem.access().unwrap();
                let _ = took.send(());
            });
            let free = taken.recv_timeout(Duration::from_secs(5)).is_ok();
            go.send(()).unwrap();
            assert_eq!(serving.join().unwrap(), 16);
            free
        });
        assert!(free, "guest memory was held while the share answered");
        assert_eq!(reply(&mem), (16, 0, 0x1234));
        drop((slot, go));
        server.join().unwrap();
    }
}

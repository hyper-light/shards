//! virtio-fs (virtio 1.3 §5.11): a host directory the guest mounts by its tag, its FUSE
//! requests answered by [`server::Server`] in a process of its own (the share process,
//! D38), reached over a Unix socket: the VM process, confined to its own files (D30),
//! reaches no directory it is given after it starts. Two queues, as Linux's driver takes
//! one high priority queue and the request queues the device says it has (here one):
//! FORGETs and INTERRUPTs come on the first, everything else on the second.

pub mod headroom;
pub mod host;
pub mod server;
pub mod window;

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
    /// Its device's DAX window (D189), and the mappings a restored device's guest held
    /// there (D176), which it maps again from the server it is given next, once that server
    /// has its tables: before the guest runs, as a restored run's shares are given first.
    window: std::sync::Mutex<Option<Arc<window::Window>>>,
    mappings: std::sync::Mutex<Vec<(u64, window::Mapped)>>,
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
/// A control frame's ask: the server's tables, or that it take them; or the descriptor of a
/// file it holds open, for a DAX window's mapping of it (D189), which comes back as a
/// shards_ipc message, the descriptor with it.
const SAVE: u8 = 1;
const RESTORE: u8 = 2;
const DESCRIPTOR: u8 = 3;
/// The requests a device with a DAX window answers itself (include/uapi/linux/fuse.h), and
/// a mapping's flag that it may be written.
const SETUPMAPPING: u32 = 48;
const REMOVEMAPPING: u32 = 49;
const MAP_WRITE: u64 = 1 << 0;
/// The most a control frame holds: a share's tables, which a snapshot's state holds.
const MAX_CONTROL: usize = server::MAX_TABLES;

impl Slot {
    /// A slot no server is in yet.
    pub const fn new() -> Slot {
        Slot {
            conn: std::sync::Mutex::new(None),
            pending: std::sync::Mutex::new(None),
            window: std::sync::Mutex::new(None),
            mappings: std::sync::Mutex::new(Vec::new()),
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
        let mappings = std::mem::take(
            &mut *self
                .mappings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if !mappings.is_empty() {
            let window = self
                .window
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .ok_or("DAX mappings for a share with no window")?;
            for (moffset, m) in mappings {
                let again = descriptor_on(&mut conn, m.node, m.writable, m.foffset, m.len)
                    .and_then(|fd| window.map(fd, m.node, m.foffset, moffset, m.len, m.writable));
                // Its file reached no more: zeros there, as past a file's end.
                if again.is_err() {
                    window
                        .map_zeros(m.node, m.foffset, moffset, m.len, m.writable)
                        .map_err(|errno| format!("a DAX range of its share: errno {errno}"))?;
                }
            }
        }
        *self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(conn);
        Ok(())
    }

    /// Its device's DAX window (D189), none where it has none.
    pub fn window(&self) -> Option<Arc<window::Window>> {
        self.window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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

    /// A descriptor of the file of its server's node `node`, for a DAX window's mapping of
    /// `len` bytes of it from `foffset` (D189), writable only if `writable`: an errno where
    /// the server refuses it, or there is none.
    fn descriptor(
        &self,
        node: u64,
        writable: bool,
        foffset: u64,
        len: u64,
    ) -> Result<std::os::fd::OwnedFd, i32> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        descriptor_on(conn.as_mut().ok_or(libc::ENODEV)?, node, writable, foffset, len)
    }

    /// The server's reply to `req`, none for one that takes none; Err with no server, or
    /// with one that has gone. One request at a time: each queue's worker asks in turn.
    #[cfg(test)]
    fn ask(&self, req: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let mut reply = Vec::new();
        Ok(self.ask_into(req, &mut reply)?.then_some(reply))
    }

    /// [`ask`](Self::ask), the reply read into `reply`, a buffer its asker keeps from one
    /// request to the next: false for a request that takes none.
    fn ask_into(&self, req: &[u8], reply: &mut Vec<u8>) -> io::Result<bool> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = conn.as_mut().ok_or(io::ErrorKind::NotConnected)?;
        write_frame(conn, req)?;
        if !read_frame_into(conn, reply)? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(!reply.is_empty())
    }
}

/// The descriptor of the file of node `node` of the server at the other end of `conn`,
/// for a DAX window's mapping of `len` bytes of it from `foffset` (D189), writable only if
/// `writable`: an errno where the server refuses it.
fn descriptor_on(
    conn: &mut UnixStream,
    node: u64,
    writable: bool,
    foffset: u64,
    len: u64,
) -> Result<std::os::fd::OwnedFd, i32> {
    write_control(conn, &descriptor_ask(node, writable, foffset, len)).map_err(|_| libc::EIO)?;
    match shards_ipc::recv(conn) {
        Ok(Some(m)) if m.kind == 0 => m.fds.into_iter().next().ok_or(libc::EIO),
        Ok(Some(m)) => Err(m
            .payload
            .get(..4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map_or(libc::EIO, i32::from_le_bytes)),
        _ => Err(libc::EIO),
    }
}

/// A `DESCRIPTOR` ask: the node (le64), whether the mapping is writable (one byte), then
/// the range of the file it maps, its offset and length (le64 each). A server that keeps
/// a file's bytes in an object of its own for each mapping (slates' shares) answers for
/// that range; shards' own reads the node and the flag alone.
fn descriptor_ask(node: u64, writable: bool, foffset: u64, len: u64) -> Vec<u8> {
    let mut ask = Vec::with_capacity(26);
    ask.push(DESCRIPTOR);
    ask.extend_from_slice(&node.to_le_bytes());
    ask.push(u8::from(writable));
    ask.extend_from_slice(&foffset.to_le_bytes());
    ask.extend_from_slice(&len.to_le_bytes());
    ask
}

/// Sends `bytes` as a frame: its length (le32), then itself, from where they are.
pub fn write_frame(to: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    to.write_all(&len.to_le_bytes())?;
    to.write_all(bytes)
}

/// The next frame, none at the end; Err for one past [`MAX_FRAME`].
pub fn read_frame(from: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    Ok(read_frame_into(from, &mut bytes)?.then_some(bytes))
}

/// [`read_frame`] into `bytes`, which it empties first, a buffer its caller keeps: false at
/// the end. A MiB read through the share otherwise made a MiB of fresh memory a reply,
/// which its host charged its run (PM M314).
fn read_frame_into(from: &mut UnixStream, bytes: &mut Vec<u8>) -> io::Result<bool> {
    let mut len = [0u8; 4];
    match from.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {len}-byte frame"),
        ));
    }
    bytes.clear();
    bytes.resize(len, 0);
    from.read_exact(bytes)?;
    Ok(true)
}

/// The bytes of the host's storage a request reads or writes, which its microVM's I/O
/// limits charge before it is asked (D178): a READ's asked for, a WRITE's given; none of
/// anything else it asks. A DAX mapping is charged as it is made, its file's bytes in its
/// range (`mapping`, D189).
fn io_bytes(req: &[u8]) -> u64 {
    let word = |at: usize| {
        req.get(at..at.saturating_add(4))
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    match word(4) {
        // fuse_read_in after the header: fh, offset, then size.
        Some(server::op::READ) => word(server::IN_HEADER_BYTES + 16).map_or(0, u64::from),
        // fuse_write_in's 40 bytes after the header, then the data.
        Some(server::op::WRITE) => {
            u64::try_from(req.len().saturating_sub(server::IN_HEADER_BYTES + 40)).unwrap_or(u64::MAX)
        }
        // A DAX window's range among them (D189), which the guest then reads and writes as
        // memory: the I/O it stands for is its file's bytes in the range, which its file's
        // size decides as it is mapped (`mapping`), not the range's length, 2 MiB for a
        // small file's too.
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
        Some(Frame::Request) => Err("an answer that is no control frame's".into()),
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

/// A frame as the VM process reads its control's answer: the answer, or a request's,
/// which no control is answered by.
enum Frame {
    Request,
    Control(Vec<u8>),
}

/// The next frame, none at the end; Err for one past its kind's most.
fn read_any(from: &mut UnixStream) -> io::Result<Option<Frame>> {
    let mut bytes = Vec::new();
    Ok(read_any_into(from, &mut bytes)?.map(|control| {
        if control {
            Frame::Control(bytes)
        } else {
            Frame::Request
        }
    }))
}

/// [`read_any`] into `bytes`, which it empties first, a buffer its caller keeps: whether
/// the frame is a control frame, none at the end.
fn read_any_into(from: &mut UnixStream, bytes: &mut Vec<u8>) -> io::Result<Option<bool>> {
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
    bytes.clear();
    bytes
        .try_reserve_exact(len)
        .map_err(|e| io::Error::other(format!("a {len}-byte frame: {e}")))?;
    bytes.resize(len, 0);
    from.read_exact(bytes)?;
    Ok(Some(control))
}

/// Answers `conn`'s requests with `server` until it closes: the share process's side.
/// A request that takes no reply gets an empty frame, so that the asker waits on none. A
/// control frame, the VM process's own, asks for the server's tables, or gives them
/// (D176), and is answered with one: 0 then what it asked for, or 1 then why not; or asks
/// for a file's descriptor (D189), answered as a shards_ipc message: 0 and the
/// descriptor, or 1 and the errno that refused it.
pub fn answer(server: &server::Server, mut conn: UnixStream) -> io::Result<()> {
    // Each request read into, and each reply built in, a buffer kept from one to the next
    // (`Server::handle_into`).
    let (mut frame, mut reply) = (Vec::new(), Vec::new());
    while let Some(control) = read_any_into(&mut conn, &mut frame)? {
        let asked = frame.as_slice();
        match control {
            false => {
                if !server.handle_into(asked, &mut reply) {
                    reply.clear();
                }
                write_frame(&mut conn, &reply)?;
            }
            // The node and whether writable; the range past them is a server's of its own
            // objects to read ([`descriptor_ask`]).
            true if asked.first() == Some(&DESCRIPTOR) => {
                let node = asked
                    .get(1..9)
                    .and_then(|b| <[u8; 8]>::try_from(b).ok())
                    .map(u64::from_le_bytes);
                let writable = asked.get(9).is_some_and(|&w| w != 0);
                match node
                    .ok_or(libc::EINVAL)
                    .and_then(|node| server.descriptor(node, writable))
                {
                    Ok(fd) => {
                        use std::os::fd::AsFd as _;
                        shards_ipc::send(&conn, 0, &[], &[fd.as_fd()])?;
                    }
                    Err(errno) => shards_ipc::send(&conn, 1, &errno.to_le_bytes(), &[])?,
                }
            }
            true => {
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
    /// which its guest maps the files it reads into, its head, region [`HEAD`], and its held
    /// record, region [`HELD`]; and what maps them there; none where it has none.
    window: Option<([ShmRegion; 3], Arc<window::Window>)>,
}

/// A DAX window (D189), and where its guest has its head and its held record, `(gpa, len)`
/// each ([`HEAD`], [`HELD`]).
pub type PlacedWindow = (Arc<window::Window>, (u64, u64), (u64, u64));

/// The cache's shared memory region's id (include/uapi/linux/virtio_fs.h).
pub const CACHE: u8 = 0;
/// Its head's (D189): a region of the device's own right before its cache, which the
/// guest's kernel keeps the cache's page structures in rather than its RAM (shards' kernel
/// patch 0008, VIRTIO_FS_SHMCAP_ID_SHARDS_HEAD).
pub const HEAD: u8 = 0xfe;
/// Its held record's (D189): which of the cache's pages its host holds, one bit a page,
/// which the guest's kernel writes whole pages its host does not hold as FUSE_WRITEs by
/// (shards' kernel patch 0009, VIRTIO_FS_SHMCAP_ID_SHARDS_HELD).
pub const HELD: u8 = 0xfd;

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

    /// The device with DAX window `window` (D189), where one is given, the window's head
    /// right before it, `(gpa, len)` ([`HEAD`]), and its held record right after it
    /// ([`HELD`]).
    pub fn with_window(mut self, window: Option<PlacedWindow>) -> Fs {
        *self
            .server
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = window.as_ref().map(|(w, _, _)| w.clone());
        self.window = window.map(|(w, (head, head_len), (held, held_len))| {
            let regions = [
                ShmRegion {
                    id: CACHE,
                    base: w.gpa(),
                    len: w.size(),
                },
                ShmRegion {
                    id: HEAD,
                    base: head,
                    len: head_len,
                },
                ShmRegion {
                    id: HELD,
                    base: held,
                    len: held_len,
                },
            ];
            (regions, w)
        });
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
            let window = self.window.as_ref().map(|(_, w)| w.clone());
            let mut buffers = Buffers::default();
            let answer =
                move |chain: &Chain| serve_with(chain, &mem, &server, limit, window.as_deref(), &mut buffers);
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
#[cfg(test)]
fn serve(
    chain: &Chain,
    mem: &GuestMemory,
    slot: &Slot,
    limit: Option<&Limiter>,
    window: Option<&window::Window>,
) -> u32 {
    serve_with(chain, mem, slot, limit, window, &mut Buffers::default())
}

/// A queue worker's buffers, kept from one request to the next: its request, read from
/// the guest, and its server's reply, which a MiB read and written would otherwise make
/// anew, fresh memory its host charges its run each time (PM M314).
#[derive(Debug, Default)]
struct Buffers {
    req: Vec<u8>,
    reply: Vec<u8>,
}

/// [`serve`], its request and reply in `buffers`.
fn serve_with(
    chain: &Chain,
    mem: &GuestMemory,
    slot: &Slot,
    limit: Option<&Limiter>,
    window: Option<&window::Window>,
    buffers: &mut Buffers,
) -> u32 {
    // Every FUSE request fits a frame, the largest write with its headers: a chain that
    // claims more, up to 256 descriptors of 4 GiB, is answered EINVAL from its header,
    // with nothing allocated for the rest (audit V03).
    let total = chain
        .readable()
        .try_fold(0usize, |n, d| n.checked_add(d.len as usize))
        .filter(|&n| n <= MAX_FRAME);
    let Buffers { req, reply } = buffers;
    req.clear();
    req.resize(total.unwrap_or(server::HEADER_BYTES), 0);
    let Ok(access) = mem.access() else {
        return 0;
    };
    let read = gather(chain, &access, req);
    drop(access);
    if read < req.len() {
        // Bytes its device could not read where the guest said they were (outside its
        // memory, or device memory whose pages are gone): told EIO where its header was
        // read, as a device whose DMA failed, never served short.
        debug!("virtio-fs: a request outside guest memory");
        let failed = req.get(..read).and_then(server::unreachable);
        return failed.map_or(0, |reply| scatter(chain, mem, &reply).unwrap_or(0));
    }
    // Charged to its microVM's I/O limits before it is asked (D178); a DAX mapping its
    // window makes as it is made, once its file's bytes in its range are known (D189).
    let maps = window.is_some()
        && req
            .get(4..8)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
            == Some(SETUPMAPPING);
    if let (Some(limit), Some(_), false) = (limit, total, maps) {
        limit.take(io_bytes(req));
    }
    // A reply of the device's own, into the buffer, which keeps what it holds room for.
    let put = |reply: &mut Vec<u8>, bytes: Vec<u8>| {
        reply.clear();
        reply.extend_from_slice(&bytes);
        true
    };
    let replied = match total {
        // A DAX window's mappings, the device's own to make (D189); anything else its
        // server's. No directory, or its server gone: the guest is told so (ENODEV), as
        // for a share it may not mount.
        Some(_) => match window.and_then(|w| mapping(req, w, slot)) {
            Some((mapped, bytes)) => {
                if let Some(limit) = limit.filter(|_| maps) {
                    limit.take(bytes);
                }
                put(reply, mapped)
            }
            None => {
                let asked = match slot.ask_into(req, reply) {
                    Ok(asked) => asked,
                    Err(_) => server::unattached(req).is_some_and(|r| put(reply, r)),
                };
                // The files whose sizes the reply tells the guest are what its window maps
                // of them before it hears: grown, the file's pages; cut short, zeros.
                if let Some(w) = window.filter(|_| asked) {
                    w.reconcile(&told(req, reply));
                    keep_mapped(req, reply, w);
                    rerecord_written(req, reply, w);
                }
                asked
            }
        },
        None => server::too_long(req).is_some_and(|r| put(reply, r)),
    };
    if !replied {
        return 0;
    }
    // A reply its device could not write whole where the guest said (device memory whose
    // pages are gone, say): EIO in its place, never a success whose bytes are not there.
    scatter(chain, mem, reply)
        .or_else(|_| server::unreachable(req).map_or(Ok(0), |e| scatter(chain, mem, &e)))
        .unwrap_or(0)
}

/// Writes `reply` into the chain's writable descriptors, in order: how many bytes, or
/// Err where one it needs could not be written.
fn scatter(chain: &Chain, mem: &GuestMemory, reply: &[u8]) -> Result<u32, ()> {
    let access = mem.access().map_err(|_| ())?;
    let mut rest = reply;
    let mut written = 0u32;
    for d in chain.writable() {
        if rest.is_empty() {
            break;
        }
        let n = rest.len().min(d.len as usize);
        let (chunk, tail) = rest.split_at(n);
        access.write(d.addr, chunk).map_err(|_| ())?;
        written = written.saturating_add(n as u32);
        rest = tail;
    }
    Ok(written)
}

/// A FUSE_SETUPMAPPING or FUSE_REMOVEMAPPING, answered from `window` (D189): a range of the
/// file of the request's node mapped there, from a descriptor its server opens, or ranges
/// unmapped; none where `req` is neither. Answered with its out header alone, its error 0
/// or an errno, and the file's bytes the range it mapped holds, which its microVM's I/O
/// limits charge it (D178): none where it mapped none.
fn mapping(req: &[u8], window: &window::Window, slot: &Slot) -> Option<(Vec<u8>, u64)> {
    let word = |at: usize| {
        req.get(at..at.checked_add(4)?)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    let long = |at: usize| {
        req.get(at..at.checked_add(8)?)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
    };
    let opcode = word(4)?;
    if opcode != SETUPMAPPING && opcode != REMOVEMAPPING {
        return None;
    }
    let unique = long(8).unwrap_or(0);
    let at = server::IN_HEADER_BYTES;
    let done = if opcode == SETUPMAPPING {
        // fuse_setupmapping_in: fh, foffset, len, flags, moffset. Linux's guest gives no
        // handle (-1), the file its header's node (fs/fuse/dax.c).
        match (
            long(16),
            long(at + 8),
            long(at + 16),
            long(at + 24),
            long(at + 32),
        ) {
            (Some(node), Some(foffset), Some(len), Some(flags), Some(moffset)) => {
                let writable = flags & MAP_WRITE != 0;
                // A descriptor its server opens for each mapping, as virtiofsd's does: each
                // range's host faults then read around them as a file opened anew does, where
                // one open of the file for all its ranges stops reading around after a
                // hundred misses, and the window's 1 MiB writes after a page of each range
                // was written took 1,285 µs at p50 where they took 682 (PM M302). The window
                // keeps one a file, not one a range.
                slot.descriptor(node, writable, foffset, len)
                    .and_then(|fd| window.map(fd, node, foffset, moffset, len, writable))
            }
            _ => Err(libc::EINVAL),
        }
    } else {
        // fuse_removemapping_in's count, then each fuse_removemapping_one: moffset, len.
        match word(at) {
            Some(count) => (0..count as usize)
                .try_for_each(|i| {
                    let one = at + 4 + i * 16;
                    match (long(one), long(one + 8)) {
                        (Some(moffset), Some(len)) => window.unmap(moffset, len),
                        _ => Err(libc::EINVAL),
                    }
                })
                .map(|()| 0),
            None => Err(libc::EINVAL),
        }
    };
    let bytes = done.unwrap_or(0);
    let error = done.err().map_or(0, |errno| errno.saturating_neg());
    let mut reply = Vec::with_capacity(16);
    reply.extend_from_slice(&16u32.to_le_bytes());
    reply.extend_from_slice(&error.to_le_bytes());
    reply.extend_from_slice(&unique.to_le_bytes());
    Some((reply, bytes))
}

/// The nodes whose files' sizes `reply`, the server's to `req`, may tell the guest
/// (include/uapi/linux/fuse.h): the request's own node, and each entry a reply gives, the
/// attributes of a LOOKUP's, CREATE's, MKNOD's, MKDIR's, SYMLINK's and LINK's, and every
/// READDIRPLUS entry's. The server serves no COPY_FILE_RANGE, TMPFILE nor STATX, whose
/// replies would tell more (ENOSYS: the guest copies, makes and asks otherwise).
fn told(req: &[u8], reply: &[u8]) -> Vec<u64> {
    use server::op::{CREATE, LINK, LOOKUP, MKDIR, MKNOD, READDIRPLUS, SYMLINK};
    let long = |b: &[u8], at: usize| {
        b.get(at..at.checked_add(8)?)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
    };
    let word = |b: &[u8], at: usize| {
        b.get(at..at.checked_add(4)?)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    let mut nodes: Vec<u64> = long(req, 16).into_iter().collect();
    // fuse_out_header: len, error, unique; an error tells nothing of an entry.
    const OUT_HEADER: usize = 16;
    // fuse_entry_out: nodeid, generation, entry_valid, attr_valid, their nsecs, fuse_attr.
    const ENTRY_OUT: usize = 128;
    // fuse_dirent: ino, off, namelen, type, then the name, padded to 8 bytes.
    const DIRENT: usize = 24;
    let opcode = word(req, 4).unwrap_or(0);
    let failed = word(reply, 4).is_none_or(|e| e != 0);
    match opcode {
        _ if failed => {}
        LOOKUP | SYMLINK | MKNOD | MKDIR | LINK | CREATE => nodes.extend(long(reply, OUT_HEADER)),
        READDIRPLUS => {
            let mut at = OUT_HEADER;
            while let (Some(node), Some(len)) = (
                long(reply, at),
                at.checked_add(ENTRY_OUT + 16).and_then(|n| word(reply, n)),
            ) {
                if node != 0 {
                    nodes.push(node);
                }
                let Some(next) = (DIRENT + len as usize)
                    .checked_next_multiple_of(8)
                    .and_then(|d| at.checked_add(ENTRY_OUT + d))
                else {
                    break;
                };
                at = next;
            }
        }
        _ => {}
    }
    nodes
}

/// Tells the guest to keep what it has of a file its window maps as it opens it again
/// (FOPEN_KEEP_CACHE, in the OPEN's reply, or the CREATE's of a file there already): Linux
/// drops a file's cached pages at each open without it (fs/fuse/file.c, `fuse_open`),
/// and for a file it maps through the window, those are its DAX entries, which it lets go
/// of while its processes still map them, 2 MiB ones split as they are (fs/dax.c,
/// `__dax_invalidate_entry`, `dax_folio_put`): the pages' counts go wrong, and its
/// processes' later faults there end in SIGBUS (PM M274). Such a file has no page cache
/// of the guest's to go stale: its bytes are the host's page cache through the window.
fn keep_mapped(req: &[u8], reply: &mut [u8], window: &window::Window) {
    // fuse_open_out's open_flags: after fh, past the out header, and a CREATE's entry.
    const OPEN_FLAGS: usize = 16 + 8;
    const ENTRY_OUT: usize = 128;
    const KEEP_CACHE: u32 = 1 << 1;
    let long = |b: &[u8], at: usize| {
        b.get(at..at.checked_add(8)?)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
    };
    let opcode = req
        .get(4..8)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes);
    if reply.get(4..8).is_none_or(|e| e != [0; 4]) {
        return;
    }
    let (node, at) = match opcode {
        Some(server::op::OPEN) => (long(req, 16), OPEN_FLAGS),
        Some(server::op::CREATE) => (long(reply, 16), ENTRY_OUT + OPEN_FLAGS),
        _ => return,
    };
    let Some(flags) = reply.get_mut(at..at.saturating_add(4)) else {
        return;
    };
    if node.is_some_and(|n| window.maps(n))
        && let Ok(bytes) = <[u8; 4]>::try_from(&*flags)
    {
        flags.copy_from_slice(&(u32::from_le_bytes(bytes) | KEEP_CACHE).to_le_bytes());
    }
}

/// Has the window record again which pages its host holds of the ranges it maps of a file
/// a FUSE_WRITE wrote, once the write is done ([`window::Window::rerecord`]): a guest's
/// kernel with patch 0009 sends a write of whole pages its host does not hold as one, by
/// the window's record, which goes stale as the host reads around a fault (D189).
fn rerecord_written(req: &[u8], reply: &[u8], window: &window::Window) {
    // fuse_in_header, 40 bytes, its nodeid at 16; then fuse_write_in: fh, offset, size.
    const IN_HEADER: usize = 40;
    let long = |at: usize| {
        req.get(at..at.checked_add(8)?)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
    };
    let word = |b: &[u8], at: usize| {
        b.get(at..at.checked_add(4)?)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    if word(req, 4) != Some(server::op::WRITE) || word(reply, 4) != Some(0) {
        return;
    }
    if let (Some(node), Some(offset), Some(size)) = (long(16), long(IN_HEADER + 8), word(req, IN_HEADER + 16))
    {
        window.rerecord(node, offset, u64::from(size));
    }
}

/// Fills `buf` with the chain's readable bytes, in order, as far as they go: how many,
/// fewer than `buf` holds where one it needs could not be read (outside guest memory, or
/// device memory whose pages are gone).
fn gather(chain: &Chain, mem: &Access<'_>, buf: &mut [u8]) -> usize {
    let mut at = 0;
    for d in chain.readable() {
        let Some(dst) = buf.get_mut(at..).filter(|rest| !rest.is_empty()) else {
            break;
        };
        let n = dst.len().min(d.len as usize);
        if dst.get_mut(..n).is_none_or(|dst| mem.read(d.addr, dst).is_err()) {
            return at;
        }
        at += n;
    }
    at
}

impl VirtioDevice for Fs {
    fn shm_regions(&self) -> &[ShmRegion] {
        self.window.as_ref().map_or(&[], |(r, _)| r.as_slice())
    }

    /// Its DAX window, whose ranges hold the files its guest maps: not its head nor its
    /// held record, whose pages no file of the guest's is.
    fn device_memory(&self) -> Vec<(u64, *mut u8, usize)> {
        self.window
            .iter()
            .filter_map(|(_, w)| Some((w.gpa(), w.host(), usize::try_from(w.size()).ok()?)))
            .collect()
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
        // Its driver's mappings went with it.
        if let Some((_, w)) = &self.window {
            w.clear();
        }
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

    /// Memory its DAX window maps that the host could not give: a range whose file another
    /// process cut short under the guest's mapping (D189, PM M271).
    fn memory_fault(&self, gpa: Option<u64>) -> bool {
        self.window.as_ref().is_some_and(|(_, w)| w.memory_fault(gpa))
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
    /// it had none, a template's; and its DAX window's mappings (D189), which the restore
    /// maps again from that server.
    fn save(&self, w: &mut Writer) {
        let tables = self
            .tables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        w.bytes(tables.as_deref().unwrap_or_default());
        let mappings = self
            .window
            .as_ref()
            .map(|(_, window)| window.mappings())
            .unwrap_or_default();
        w.seq(&mappings, |w, (at, m)| {
            w.u64(*at);
            w.u64(m.len);
            w.u64(m.node);
            w.u64(m.foffset);
            w.bool(m.writable);
        });
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
        // As many as its window has ranges: none without one.
        let most = self.window.as_ref().map_or(0, |(_, w)| {
            usize::try_from(w.size() / window::RANGE).unwrap_or(usize::MAX)
        });
        let mappings = r.seq(most, 33, |r| {
            let at = r.u64()?;
            Ok((
                at,
                window::Mapped {
                    len: r.u64()?,
                    node: r.u64()?,
                    foffset: r.u64()?,
                    writable: r.bool()?,
                },
            ))
        })?;
        *self
            .server
            .mappings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mappings;
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

    /// What a request costs its microVM's I/O limits before it is asked (D178): a READ the
    /// bytes it asks for, after its 40-byte header; a WRITE the bytes it gives; anything else
    /// nothing, a DAX mapping among them, charged as it is made its file's bytes in its range
    /// (`a_dax_mapping_maps_the_file_of_its_node`, D189).
    #[test]
    fn requests_are_charged_what_they_read_and_write() {
        let request = |opcode: u32, args: &[u8]| {
            let mut r = Vec::new();
            r.extend((40 + args.len() as u32).to_le_bytes());
            r.extend(opcode.to_le_bytes());
            r.extend(9u64.to_le_bytes());
            r.extend(1u64.to_le_bytes());
            // uid, gid and a pid of 4242, which a READ is not charged by.
            r.extend([0u8; 8]);
            r.extend(4242u32.to_le_bytes());
            r.extend([0u8; 4]);
            r.extend(args);
            r
        };
        let mut read = 7u64.to_le_bytes().to_vec();
        read.extend(0u64.to_le_bytes());
        read.extend(65536u32.to_le_bytes());
        read.extend([0u8; 20]);
        assert_eq!(io_bytes(&request(server::op::READ, &read)), 65536);
        let mut write = vec![0u8; 40];
        write.extend([7u8; 4096]);
        assert_eq!(io_bytes(&request(server::op::WRITE, &write)), 4096);
        let mut setup = Vec::new();
        for v in [7u64, 0, 2 << 20, 2, 0] {
            setup.extend(v.to_le_bytes());
        }
        assert_eq!(io_bytes(&request(SETUPMAPPING, &setup)), 0);
        assert_eq!(io_bytes(&request(1, b"f\0")), 0);
    }

    /// A DAX window's mappings are the device's (D189): a FUSE_SETUPMAPPING, which names its
    /// file by its node alone as Linux's guest sends it, maps the range of that file into
    /// the window, from a descriptor its share's server opens and sends, writable where it
    /// asks, charged its microVM's I/O limits the file's bytes in the range (D178): a file
    /// shorter than its range, 5,000 bytes of 2 MiB, its 5,000, and a range past its file's
    /// end nothing; a directory's is refused, and charged nothing; a FUSE_REMOVEMAPPING
    /// unmaps; anything else goes to the server.
    #[test]
    fn a_dax_mapping_maps_the_file_of_its_node() {
        struct Nothing;
        impl window::Stage2 for Nothing {
            unsafe fn map(&self, _: *mut u8, _: u64, _: usize, _: bool) -> Result<(), String> {
                Ok(())
            }
            fn unmap(&self, _: u64, _: usize) -> Result<(), String> {
                Ok(())
            }
        }
        const MIB: u64 = 1 << 20;
        let dir = shards_testdir::TempDir::new("fs-dax").unwrap();
        let bytes: Vec<u8> = (0..4 * MIB).map(|i| (i / 4096 % 251) as u8).collect();
        std::fs::write(dir.join("big"), &bytes).unwrap();
        std::fs::write(dir.join("small"), &bytes[..5000]).unwrap();
        let server = server::Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || answer(&server, theirs).unwrap());
        let slot = Slot::new();
        slot.attach(ours).unwrap();
        let request = |opcode: u32, nodeid: u64, body: &[u8]| {
            let mut r = Vec::new();
            r.extend((40 + body.len() as u32).to_le_bytes());
            r.extend(opcode.to_le_bytes());
            r.extend(9u64.to_le_bytes());
            r.extend(nodeid.to_le_bytes());
            r.extend([0u8; 16]);
            r.extend(body);
            r
        };
        let looked = slot.ask(&request(1, 1, b"big\0")).unwrap().unwrap();
        assert_eq!(
            i32::from_le_bytes(looked[4..8].try_into().unwrap()),
            0,
            "{looked:?}"
        );
        let node = u64::from_le_bytes(looked[16..24].try_into().unwrap());
        let small = slot.ask(&request(1, 1, b"small\0")).unwrap().unwrap();
        let small = u64::from_le_bytes(small[16..24].try_into().unwrap());
        let w = window::Window::new(1 << 32, 8 * MIB, |_| {
            Ok(Box::new(Nothing) as Box<dyn window::Stage2>)
        })
        .unwrap();
        // fuse_setupmapping_in as Linux's guest sends it: no handle, 2 MiB of the file from
        // `foffset`; FUSE_SETUPMAPPING_FLAG_READ, and _WRITE where asked.
        let setup_at = |node: u64, write: bool, moffset: u64, foffset: u64| {
            let mut body = Vec::new();
            let flags = 2 | if write { MAP_WRITE } else { 0 };
            for v in [u64::MAX, foffset, 2 * MIB, flags, moffset] {
                body.extend(v.to_le_bytes());
            }
            request(SETUPMAPPING, node, &body)
        };
        let setup = |node: u64, write: bool, moffset: u64| setup_at(node, write, moffset, 2 * MIB);
        let error = |reply: Vec<u8>| i32::from_le_bytes(reply[4..8].try_into().unwrap());
        let (reply, charged) = mapping(&setup(node, false, 4 * MIB), &w, &slot).unwrap();
        assert_eq!((error(reply), charged), (0, 2 * MIB));
        // SAFETY: the window's own range, the file mapped there, for as long as `w` is.
        let seen = unsafe { std::slice::from_raw_parts(w.host().add(4 << 20), 2 << 20) };
        assert_eq!(seen, &bytes[2 << 20..4 << 20]);
        assert_eq!(w.mappings().len(), 1);
        // Writable: what the guest writes there is the file's.
        assert_eq!(error(mapping(&setup(node, true, 0), &w, &slot).unwrap().0), 0);
        // SAFETY: the window's own range, mapped writable above.
        unsafe { w.host().write_volatile(!bytes[2 << 20]) };
        assert_eq!(std::fs::read(dir.join("big")).unwrap()[2 << 20], !bytes[2 << 20]);
        let (reply, charged) = mapping(&setup(1, false, 6 * MIB), &w, &slot).unwrap();
        assert_eq!((error(reply), charged), (-libc::EISDIR, 0));
        let (reply, charged) = mapping(&setup_at(small, false, 6 * MIB, 0), &w, &slot).unwrap();
        assert_eq!((error(reply), charged), (0, 5000));
        let (reply, charged) = mapping(&setup_at(node, false, 6 * MIB, 4 * MIB), &w, &slot).unwrap();
        assert_eq!((error(reply), charged), (0, 0));
        let mut gone = 3u32.to_le_bytes().to_vec();
        for moffset in [4 * MIB, 0, 6 * MIB] {
            gone.extend(moffset.to_le_bytes());
            gone.extend((2 * MIB).to_le_bytes());
        }
        let (reply, charged) = mapping(&request(REMOVEMAPPING, node, &gone), &w, &slot).unwrap();
        assert_eq!((error(reply), charged), (0, 0));
        assert!(w.mappings().is_empty());
        assert!(mapping(&request(1, 1, b"big\0"), &w, &slot).is_none());
        drop(slot);
        t.join().unwrap();
    }

    /// A DAX mapping is charged its microVM's I/O limits as it is made, its file's bytes in
    /// its range, once (D178, D189): at 1 MiB a second, ten mappings of a 5,000-byte file's
    /// range pass in milliseconds, where the range's 2 MiB each would hold them 18 s, and a
    /// request after a whole range's mapping waits the 2 s its 2 MiB take.
    #[test]
    fn a_dax_mapping_is_charged_its_files_bytes_as_it_is_made() {
        struct Nothing;
        impl window::Stage2 for Nothing {
            unsafe fn map(&self, _: *mut u8, _: u64, _: usize, _: bool) -> Result<(), String> {
                Ok(())
            }
            fn unmap(&self, _: u64, _: usize) -> Result<(), String> {
                Ok(())
            }
        }
        const MIB: u64 = 1 << 20;
        let dir = shards_testdir::TempDir::new("fs-dax-charge").unwrap();
        std::fs::write(dir.join("big"), vec![1u8; 4 << 20]).unwrap();
        std::fs::write(dir.join("small"), [1u8; 5000]).unwrap();
        let server = server::Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || answer(&server, theirs).unwrap());
        let slot = Slot::new();
        slot.attach(ours).unwrap();
        let request = |opcode: u32, nodeid: u64, body: &[u8]| {
            let mut r = Vec::new();
            r.extend((40 + body.len() as u32).to_le_bytes());
            r.extend(opcode.to_le_bytes());
            r.extend(9u64.to_le_bytes());
            r.extend(nodeid.to_le_bytes());
            r.extend([0u8; 16]);
            r.extend(body);
            r
        };
        let node = |name: &[u8]| {
            let looked = slot.ask(&request(1, 1, name)).unwrap().unwrap();
            u64::from_le_bytes(looked[16..24].try_into().unwrap())
        };
        let (big, small) = (node(b"big\0"), node(b"small\0"));
        let w = window::Window::new(1 << 32, 64 * MIB, |_| {
            Ok(Box::new(Nothing) as Box<dyn window::Stage2>)
        })
        .unwrap();
        let mem = guest();
        let limit = Limiter::new();
        limit.set(MIB, 0);
        // fuse_setupmapping_in: no handle, 2 MiB of the file from 0, to read, at `moffset`.
        let map = |node: u64, moffset: u64| {
            let mut body = Vec::new();
            for v in [u64::MAX, 0, 2 * MIB, 2, moffset] {
                body.extend(v.to_le_bytes());
            }
            let req = request(SETUPMAPPING, node, &body);
            mem.access().unwrap().write(REQUEST, &req).unwrap();
            let len = u32::try_from(req.len()).unwrap();
            assert_eq!(
                serve(
                    &chain(vec![readable(REQUEST, len)]),
                    &mem,
                    &slot,
                    Some(&limit),
                    Some(&w)
                ),
                16
            );
            assert_eq!(reply(&mem).1, 0);
        };
        let start = std::time::Instant::now();
        for i in 0..10 {
            map(small, i * 2 * MIB);
        }
        let ten = start.elapsed();
        assert!(
            ten < std::time::Duration::from_secs(2),
            "ten 5,000-byte mappings took {ten:?}"
        );
        map(big, 20 * MIB);
        let after = std::time::Instant::now();
        map(small, 22 * MIB);
        let waited = after.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(1900),
            "waited {waited:?} after 2 MiB"
        );
        drop(slot);
        t.join().unwrap();
    }

    /// A `DESCRIPTOR` ask says the range of the file its mapping maps after its node and
    /// flag, bytes 10 to 26, for a server that answers each range from an object of its own
    /// (slates' shares); shards' own server reads past them, as
    /// `a_dax_mapping_maps_the_file_of_its_node` asks it so.
    #[test]
    fn a_descriptor_ask_says_its_mappings_range() {
        let ask = descriptor_ask(0x0102_0304_0506_0708, true, 2 << 20, 1 << 20);
        assert_eq!(ask.len(), 26);
        assert_eq!(ask[0], DESCRIPTOR);
        assert_eq!(ask[1..9], 0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(ask[9], 1);
        assert_eq!(ask[10..18], (2u64 << 20).to_le_bytes());
        assert_eq!(ask[18..26], (1u64 << 20).to_le_bytes());
        assert_eq!(descriptor_ask(7, false, 0, 0)[9], 0);
    }

    /// The files whose sizes a reply may tell the guest, as the server answers: its
    /// request's own node; a LOOKUP's entry; every READDIRPLUS entry's; and of a reply
    /// that failed, its request's node alone.
    #[test]
    fn a_reply_tells_its_requests_node_and_its_entries() {
        let dir = shards_testdir::TempDir::new("fs-told").unwrap();
        for name in ["a", "bb", "a-longer-name-past-eight"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let server = server::Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
        let request = |opcode: u32, nodeid: u64, body: &[u8]| {
            let mut r = Vec::new();
            r.extend((40 + body.len() as u32).to_le_bytes());
            r.extend(opcode.to_le_bytes());
            r.extend(9u64.to_le_bytes());
            r.extend(nodeid.to_le_bytes());
            r.extend([0u8; 16]);
            r.extend(body);
            r
        };
        let ask = |req: &[u8]| {
            (
                server.handle(req).unwrap(),
                told(req, &server.handle(req).unwrap()),
            )
        };
        let node_of = |reply: &[u8]| u64::from_le_bytes(reply[16..24].try_into().unwrap());
        let (looked, nodes) = ask(&request(server::op::LOOKUP, 1, b"bb\0"));
        assert_eq!(nodes, [1, node_of(&looked)]);
        let (_, nodes) = ask(&request(server::op::LOOKUP, 1, b"none\0"));
        assert_eq!(nodes, [1], "a LOOKUP that failed");
        let (_, nodes) = ask(&request(server::op::GETATTR, node_of(&looked), &[0u8; 16]));
        assert_eq!(nodes, [node_of(&looked)]);
        // fuse_open_in, then fuse_read_in: fh, offset, size, read_flags, lock_owner, flags.
        let opened = server
            .handle(&request(server::op::OPENDIR, 1, &[0u8; 8]))
            .unwrap();
        let fh = node_of(&opened);
        let mut read = Vec::new();
        read.extend(fh.to_le_bytes());
        read.extend(0u64.to_le_bytes());
        read.extend(4096u32.to_le_bytes());
        read.extend([0u8; 20]);
        let req = request(server::op::READDIRPLUS, 1, &read);
        let listed = server.handle(&req).unwrap();
        let nodes = told(&req, &listed);
        let mut want = vec![1];
        for name in ["a", "bb", "a-longer-name-past-eight"] {
            let looked = server
                .handle(&request(server::op::LOOKUP, 1, format!("{name}\0").as_bytes()))
                .unwrap();
            want.push(node_of(&looked));
        }
        let mut got = nodes.clone();
        got.sort_unstable();
        want.sort_unstable();
        assert!(want.iter().all(|n| got.contains(n)), "{got:?} for {want:?}");
        assert!(got.iter().all(|n| want.contains(n)), "{got:?} for {want:?}");
    }

    /// An OPEN of a file the window maps, or a CREATE that opens one there already, is told
    /// to keep what the guest has of it (FOPEN_KEEP_CACHE), what else its flags say kept; a
    /// file it maps none of, a reply that failed, or another request's, not.
    #[test]
    fn opening_a_file_the_window_maps_keeps_its_dax_entries() {
        struct Nothing;
        impl window::Stage2 for Nothing {
            unsafe fn map(&self, _: *mut u8, _: u64, _: usize, _: bool) -> Result<(), String> {
                Ok(())
            }
            fn unmap(&self, _: u64, _: usize) -> Result<(), String> {
                Ok(())
            }
        }
        const MIB: u64 = 1 << 20;
        let dir = shards_testdir::TempDir::new("fs-keep").unwrap();
        std::fs::write(dir.join("big"), vec![1u8; 4 << 20]).unwrap();
        let w = window::Window::new(1 << 32, 8 * MIB, |_| {
            Ok(Box::new(Nothing) as Box<dyn window::Stage2>)
        })
        .unwrap();
        let fd = std::os::fd::OwnedFd::from(std::fs::File::open(dir.join("big")).unwrap());
        w.map(fd, 7, 0, 0, 2 * MIB, false).unwrap();
        let request = |opcode: u32, nodeid: u64| {
            let mut r = Vec::new();
            r.extend(48u32.to_le_bytes());
            r.extend(opcode.to_le_bytes());
            r.extend(9u64.to_le_bytes());
            r.extend(nodeid.to_le_bytes());
            r.extend([0u8; 24]);
            r
        };
        // fuse_out_header, then fuse_open_out: fh, open_flags (FOPEN_DIRECT_IO already).
        let open_reply = |error: i32| {
            let mut r = Vec::new();
            r.extend(32u32.to_le_bytes());
            r.extend(error.to_le_bytes());
            r.extend(9u64.to_le_bytes());
            r.extend(3u64.to_le_bytes());
            r.extend(1u32.to_le_bytes());
            r.extend(0i32.to_le_bytes());
            r
        };
        let flags = |r: &[u8], at: usize| u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
        let mut r = open_reply(0);
        keep_mapped(&request(server::op::OPEN, 7), &mut r, &w);
        assert_eq!(flags(&r, 24), 1 | 2, "kept, its own flag too");
        for (req, mut r) in [
            (request(server::op::OPEN, 8), open_reply(0)),
            (request(server::op::OPEN, 7), open_reply(-libc::ENOENT)),
            (request(server::op::GETATTR, 7), open_reply(0)),
        ] {
            keep_mapped(&req, &mut r, &w);
            assert_eq!(flags(&r, 24), 1);
        }
        // A CREATE's: fuse_entry_out (nodeid first), then fuse_open_out.
        let mut created = open_reply(0)[..16].to_vec();
        created.extend(7u64.to_le_bytes());
        created.extend([0u8; 120]);
        created.extend(open_reply(0)[16..].iter());
        keep_mapped(&request(server::op::CREATE, 1), &mut created, &w);
        assert_eq!(flags(&created, 16 + 128 + 8), 1 | 2);
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

    /// A restored share's DAX mappings (D189, D176) are mapped again as its next server is
    /// given, after that server takes the tables its node IDs are in: from the file of each
    /// node, or zeros where that server reaches no file of it.
    #[test]
    fn a_restored_shares_mappings_are_mapped_again_from_its_new_server() {
        struct Nothing;
        impl window::Stage2 for Nothing {
            unsafe fn map(&self, _: *mut u8, _: u64, _: usize, _: bool) -> Result<(), String> {
                Ok(())
            }
            fn unmap(&self, _: u64, _: usize) -> Result<(), String> {
                Ok(())
            }
        }
        const MIB: u64 = 1 << 20;
        let dir = shards_testdir::TempDir::new("fs-dax-saved").unwrap();
        let bytes: Vec<u8> = (0..4 * MIB).map(|i| (i / 4096 % 251) as u8).collect();
        std::fs::write(dir.join("big"), &bytes).unwrap();
        let serve = || {
            let server = server::Server::new(std::fs::File::open(&dir).unwrap().into(), false, None).unwrap();
            let (ours, theirs) = UnixStream::pair().unwrap();
            let t = std::thread::spawn(move || answer(&server, theirs).unwrap());
            (ours, t)
        };
        let (conn, first) = serve();
        let slot = Slot::new();
        slot.attach(conn).unwrap();
        let mut lookup = Vec::new();
        lookup.extend((40u32 + 4).to_le_bytes());
        lookup.extend(1u32.to_le_bytes());
        lookup.extend(7u64.to_le_bytes());
        lookup.extend(1u64.to_le_bytes());
        lookup.extend([0u8; 16]);
        lookup.extend(b"big\0");
        let reply = slot.ask(&lookup).unwrap().unwrap();
        assert_eq!(i32::from_le_bytes(reply[4..8].try_into().unwrap()), 0);
        let node = u64::from_le_bytes(reply[16..24].try_into().unwrap());
        let tables = slot.tables().unwrap().unwrap();
        drop(slot);
        first.join().unwrap();
        // Restored: the window empty, its mappings the slot's to make.
        let w = Arc::new(
            window::Window::new(1 << 32, 8 * MIB, |_| {
                Ok(Box::new(Nothing) as Box<dyn window::Stage2>)
            })
            .unwrap(),
        );
        let restored = Slot::new();
        *restored.pending.lock().unwrap() = Some(tables);
        *restored.window.lock().unwrap() = Some(w.clone());
        let kept = |node, foffset| window::Mapped {
            len: 2 * MIB,
            node,
            foffset,
            writable: false,
        };
        *restored.mappings.lock().unwrap() = vec![(2 * MIB, kept(node, 2 * MIB)), (6 * MIB, kept(9999, 0))];
        let (conn, second) = serve();
        restored.attach(conn).unwrap();
        assert_eq!(
            w.mappings(),
            [(2 * MIB, kept(node, 2 * MIB)), (6 * MIB, kept(9999, 0))]
        );
        // SAFETY: the window's own ranges, mapped above, for as long as `w` is.
        let (file, zeros) = unsafe {
            (
                std::slice::from_raw_parts(w.host().add(2 << 20), 2 << 20),
                std::slice::from_raw_parts(w.host().add(6 << 20), 2 << 20),
            )
        };
        assert_eq!(file, &bytes[2 << 20..4 << 20]);
        assert!(zeros.iter().all(|&b| b == 0));
        drop(restored);
        second.join().unwrap();
    }

    fn share(hold: mpsc::Receiver<()>, asked: mpsc::Sender<Vec<u8>>) -> (Share, std::thread::JoinHandle<()>) {
        share_answering(hold, asked, 0)
    }

    /// [`share`], its answers `extra` bytes longer than their header.
    fn share_answering(
        hold: mpsc::Receiver<()>,
        asked: mpsc::Sender<Vec<u8>>,
        extra: usize,
    ) -> (Share, std::thread::JoinHandle<()>) {
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
                let mut out = (16 + extra as u32).to_le_bytes().to_vec();
                out.extend_from_slice(&0i32.to_le_bytes());
                out.extend_from_slice(&req[8..16]);
                out.resize(16 + extra, 0x77);
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
            assert_eq!(serve(&chain(readables), &mem, &slot, None, None), 16);
            assert_eq!(reply(&mem), (16, -22, 0x1234));
            assert!(requests.try_recv().is_err(), "the share was asked");
        }
        go.send(()).unwrap();
        assert_eq!(
            serve(&chain(vec![readable(REQUEST, 56)]), &mem, &slot, None, None),
            16
        );
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
            let serving = scope.spawn(|| serve(&chain(vec![readable(REQUEST, 56)]), &mem, &slot, None, None));
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

    /// A request whose bytes lie in device memory, a page of a file its guest maps (D189),
    /// goes to the share whole, as one in RAM does: a write from a mapped file's pages. One
    /// whose bytes, or whose reply's, its device cannot reach, a page of device memory whose
    /// file was cut short, or none at all, is answered EIO: never sent short, nor answered
    /// as done with its bytes missing.
    #[cfg(unix)]
    #[test]
    fn a_requests_device_memory_goes_whole_and_its_lost_pages_fail_it() {
        use std::os::fd::AsRawFd as _;
        const DEVICE: u64 = 0x2_0000_0000;
        let mem = guest();
        let p = crate::platform::page_size().unwrap();
        let dir = shards_testdir::TempDir::new("fs-device").unwrap();
        let bytes: Vec<u8> = (0..2 * p).map(|i| (i * 7 + 1) as u8).collect();
        std::fs::write(dir.join("f"), &bytes).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("f"))
            .unwrap();
        // SAFETY: a shared mapping of the file's two pages, unmapped at the test's end.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * p,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(host, libc::MAP_FAILED);
        // SAFETY: the mapping above, which outlives every use of `mem`'s.
        unsafe { mem.give_devices([(DEVICE, host.cast(), 2 * p)]) }.unwrap();
        let (go, hold) = mpsc::channel();
        let (asked, requests) = mpsc::channel();
        let (slot, server) = share_answering(hold, asked, 100);
        // Its bytes in device memory: sent whole, and answered.
        go.send(()).unwrap();
        let readables = vec![readable(REQUEST, 56), readable(DEVICE + 8, 100)];
        assert_eq!(serve(&chain(readables), &mem, &slot, None, None), 116);
        assert_eq!(reply(&mem), (116, 0, 0x1234));
        let sent = requests.recv().unwrap();
        assert_eq!(sent.len(), 156);
        assert_eq!(&sent[56..], &bytes[8..108]);
        // Cut short under its mapping: its lost page is not read, the share not asked.
        file.set_len(p as u64).unwrap();
        let lost = DEVICE + p as u64;
        let readables = vec![readable(REQUEST, 56), readable(lost + 8, 100)];
        assert_eq!(serve(&chain(readables), &mem, &slot, None, None), 16);
        assert_eq!(reply(&mem), (16, -5, 0x1234));
        assert!(requests.try_recv().is_err(), "the share was asked");
        // A reply whose bytes go to the lost page: EIO in its place.
        go.send(()).unwrap();
        let descriptors = vec![
            readable(REQUEST, 56),
            Descriptor {
                addr: REPLY,
                len: 16,
                writable: true,
            },
            Descriptor {
                addr: lost,
                len: 4096,
                writable: true,
            },
        ];
        let lost_reply = Chain { head: 0, descriptors };
        assert_eq!(serve(&lost_reply, &mem, &slot, None, None), 16);
        assert_eq!(reply(&mem), (16, -5, 0x1234));
        assert_eq!(requests.recv().unwrap().len(), 56);
        drop((slot, go));
        server.join().unwrap();
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(host, 2 * p) };
    }
}

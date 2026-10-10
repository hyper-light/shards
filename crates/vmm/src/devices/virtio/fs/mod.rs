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

use super::queue::{Chain, Queue};
use super::worker::Worker;
use super::{Activation, DeviceInterrupt, VirtioDevice, feature};
use crate::debug;
use crate::memory::{Access, GuestMemory};

pub const DEVICE_ID: u32 = 26;
const QUEUE_SIZE: u16 = 256;
/// The most a tag holds (virtio 1.3 §5.11.4).
pub const TAG_BYTES: usize = 36;

/// Where a share's server is reached once its directory is known: a VM restored ahead of
/// its run has its devices, and learns its directories with the run (D38).
#[derive(Debug, Default)]
pub struct Slot(std::sync::Mutex<Option<UnixStream>>);

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

impl Slot {
    /// A slot no server is in yet.
    pub const fn new() -> Slot {
        Slot(std::sync::Mutex::new(None))
    }

    /// Asks the server at the other end of `conn` from here on.
    pub fn attach(&self, conn: UnixStream) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(conn);
    }

    /// The server's reply to `req`, none for one that takes none; Err with no server, or
    /// with one that has gone. One request at a time: each queue's worker asks in turn.
    fn ask(&self, req: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let mut conn = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// Answers `conn`'s requests with `server` until it closes: the share process's side.
/// A request that takes no reply gets an empty frame, so that the asker waits on none.
pub fn answer(server: &server::Server, mut conn: UnixStream) -> io::Result<()> {
    while let Some(req) = read_frame(&mut conn)? {
        let reply = server.handle(&req).unwrap_or_default();
        write_frame(&mut conn, &reply)?;
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
}

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
        })
    }

    fn start(&mut self, queues: Vec<Queue>) -> Result<(), String> {
        let Some((memory, interrupt)) = self.context.clone() else {
            self.paused = queues;
            return Err("virtio-fs started before activation".into());
        };
        let mut queues = queues.into_iter();
        while let Some(queue) = queues.next() {
            let (server, mem) = (self.server.clone(), memory.clone());
            let answer = move |chain: &Chain| serve(chain, &mem, &server);
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
}

/// Answers one request: its readable bytes to the server, its reply into the writable
/// descriptors. The bytes written. Guest memory is held to read the request and to write
/// the reply, never while the share answers (D29): a share slow to answer, or never
/// answering, held every other device's queue work, and a snapshot, off guest memory for
/// as long (audit V04).
fn serve(chain: &Chain, mem: &GuestMemory, slot: &Slot) -> u32 {
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
    }

    fn pause(&mut self) -> Vec<super::QueueState> {
        let stopped = self.stop();
        if !stopped.is_empty() {
            self.paused = stopped;
        }
        self.paused.iter().map(Queue::state).collect()
    }

    fn resume(&mut self) -> Result<(), String> {
        let queues = std::mem::take(&mut self.paused);
        if queues.is_empty() {
            Ok(())
        } else {
            self.start(queues)
        }
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
    fn share(hold: mpsc::Receiver<()>, asked: mpsc::Sender<Vec<u8>>) -> (Share, std::thread::JoinHandle<()>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let slot = Share::default();
        slot.attach(ours);
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
            assert_eq!(serve(&chain(readables), &mem, &slot), 16);
            assert_eq!(reply(&mem), (16, -22, 0x1234));
            assert!(requests.try_recv().is_err(), "the share was asked");
        }
        go.send(()).unwrap();
        assert_eq!(serve(&chain(vec![readable(REQUEST, 56)]), &mem, &slot), 16);
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
            let serving = scope.spawn(|| serve(&chain(vec![readable(REQUEST, 56)]), &mem, &slot));
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

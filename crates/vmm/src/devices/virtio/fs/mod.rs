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
use crate::memory::GuestMemory;
use crate::warn;

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

/// The most a request or reply frame holds: a write's data and its headers.
pub const MAX_FRAME: usize = server::MAX_WRITE as usize + 4096;

impl Slot {
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
    server: Share,
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
/// descriptors. The bytes written.
fn serve(chain: &Chain, mem: &GuestMemory, slot: &Slot) -> u32 {
    let Ok(access) = mem.access() else {
        return 0;
    };
    let total: usize = chain.readable().map(|d| d.len as usize).sum();
    let mut req = vec![0u8; total];
    let mut at = 0;
    for d in chain.readable() {
        let Some(dst) = req.get_mut(at..at + d.len as usize) else {
            return 0;
        };
        if access.read(d.addr, dst).is_err() {
            warn!("virtio-fs: a request outside guest memory");
            return 0;
        }
        at += d.len as usize;
    }
    // No directory, or its server gone: the guest is told so (ENODEV), as for a share it
    // may not mount.
    let reply = match slot.ask(&req) {
        Ok(reply) => reply,
        Err(_) => server::unattached(&req),
    };
    let Some(reply) = reply else {
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

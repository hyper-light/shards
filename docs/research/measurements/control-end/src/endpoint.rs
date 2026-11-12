//! Arm h's endpoint, on the host: TLS ends here, on D180's QUIC driver and identity as Shards'
//! managed owner endpoint has them, and each stream goes on to its microVM over a vsock
//! stream of its own (the VM's socket, Firecracker's `CONNECT port`), header and all.
//!
//! The driver keeps every connection on one thread. Each vsock leg has two threads: one
//! writes what the stream carries to the guest, one reads what the guest answers and hands it
//! to the driver as a command, as the driver's other threads reach it. Each direction holds
//! one chunk ([`CHUNK`]) in flight: the driver reads no more of a stream until the guest's
//! writer has taken the last, and the reader no more of the guest until the stream has taken
//! the last.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};

use hyper_quic::StreamId;
use shards_remote::net::{Close, Command, ConnKey, Driver, Handler, Io, Read, Sender};

use crate::proto::{self, CHUNK, Header, Opts};

#[cfg(unix)]
type VsockStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type VsockStream = shards_afunix::UnixStream;

type Key = (ConnKey, StreamId);

pub fn main(args: &[String]) -> Result<(), String> {
    let opts = Opts::parse(args, &["listen", "controller", "sock-dir", "vms"])?;
    let listen: std::net::SocketAddr = opts
        .get("listen")
        .unwrap_or("127.0.0.1:0")
        .parse()
        .map_err(|e| format!("--listen: {e}"))?;
    let controller = proto::fingerprint(opts.need("controller")?)?;
    let dir = PathBuf::from(opts.need("sock-dir")?);
    let vms: u32 = opts.num("vms", 0)?;
    let socks = (0..vms).map(|i| dir.join(sock_name(i))).collect();
    crate::procs::raise_descriptor_limit();
    let (mut driver, at, fingerprint) = bind(listen, controller, socks)?;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "control-end endpoint listening={at} fingerprint={}",
        proto::hex(&fingerprint)
    );
    let _ = out.flush();
    drop(out);
    driver.run().map_err(|e| e.to_string())
}

/// The endpoint on `listen`, its key made here, for the controller whose key's SHA-256 is
/// `controller`, microVM `i`'s guest behind socket `socks[i]`; where it listens, and its key's
/// SHA-256.
pub fn bind(
    listen: std::net::SocketAddr,
    controller: [u8; 32],
    socks: Vec<PathBuf>,
) -> Result<(Driver<Endpoint>, std::net::SocketAddr, [u8; 32]), String> {
    #[cfg(windows)]
    shards_afunix::startup().map_err(|e| format!("Winsock: {e}"))?;
    let identity = shards_remote::identity::Identity::generate().map_err(|e| e.to_string())?;
    let config = shards_remote::tls::server(&identity).map_err(|e| e.to_string())?;
    let endpoint = Endpoint {
        controller,
        socks,
        relays: HashMap::new(),
        sender: None,
    };
    let (mut driver, sender) = Driver::bind(listen, Some(config), endpoint).map_err(|e| e.to_string())?;
    let at = driver.local().map_err(|e| e.to_string())?;
    driver.handler_mut().sender = Some(sender);
    Ok((driver, at, identity.fingerprint()))
}

/// The vsock socket of microVM `i` in the measurement's socket directory.
pub fn sock_name(i: u32) -> String {
    format!("v{i}.sock")
}

/// What a leg's threads tell the driver.
#[derive(Debug)]
pub enum Msg {
    /// The guest took the stream, or would not.
    Up { key: Key, legs: Result<Legs, String> },
    /// The guest's bytes, for the stream.
    FromGuest { key: Key, bytes: Vec<u8> },
    /// The guest ended its side.
    GuestEnd { key: Key },
    /// The writer has written the last chunk it was given.
    Drained { key: Key },
}

/// A stream's vsock leg, as the driver holds it.
#[derive(Debug)]
pub struct Legs {
    /// Chunks for the writer; an empty one ends the guest's reading side.
    up: SyncSender<Vec<u8>>,
    /// Lets the reader read on, once the stream has taken what it read.
    permit: SyncSender<()>,
    /// The leg's socket, to end it should the stream go first.
    socket: VsockStream,
}

impl Legs {
    fn abandon(&self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Debug, Default)]
struct Relay {
    head: Vec<u8>,
    connecting: bool,
    legs: Option<Legs>,
    /// The next chunk for the guest; an empty one is the controller's end.
    up: Option<Vec<u8>>,
    /// The controller's end follows `up`.
    end_next: bool,
    /// The controller's end went to the writer.
    up_done: bool,
    /// The guest's bytes not yet all on the stream, and how many are.
    down: Option<(Vec<u8>, usize)>,
    /// The guest ended its side.
    down_end: bool,
    /// The stream's sending side was finished.
    finished: bool,
}

#[derive(Debug)]
pub struct Endpoint {
    controller: [u8; 32],
    socks: Vec<PathBuf>,
    relays: HashMap<Key, Relay>,
    sender: Option<Sender<Msg>>,
}

impl Endpoint {
    fn drop_relay(&mut self, io: &mut Io<'_>, key: Key) {
        if let Some(relay) = self.relays.remove(&key) {
            if let Some(legs) = &relay.legs {
                legs.abandon();
            }
            if !relay.finished {
                io.reset(key.0, key.1);
            }
            io.stop(key.0, key.1);
        }
    }

    /// The stream's bytes, on to the guest.
    fn up(&mut self, io: &mut Io<'_>, key: Key) {
        let (conn, stream) = key;
        loop {
            let Some(relay) = self.relays.get_mut(&key) else {
                return;
            };
            if relay.up_done {
                return;
            }
            let Some(legs) = &relay.legs else {
                if relay.connecting {
                    return;
                }
                let want = proto::HEADER.saturating_sub(relay.head.len());
                match io.read_within(conn, stream, want) {
                    Ok(Read::Bytes(b)) => relay.head.extend_from_slice(&b),
                    Ok(Read::Blocked) => return,
                    Ok(Read::End(_) | Read::Reset) | Err(_) => {
                        self.drop_relay(io, key);
                        return;
                    }
                }
                if relay.head.len() < proto::HEADER {
                    continue;
                }
                let vm = Header::decode(&relay.head).map(|h| h.vm);
                let sock = vm.ok().and_then(|vm| self.socks.get(vm as usize).cloned());
                let (Some(sock), Some(sender)) = (sock, self.sender.as_ref()) else {
                    self.drop_relay(io, key);
                    return;
                };
                let Ok(sender) = sender.try_clone() else {
                    self.drop_relay(io, key);
                    return;
                };
                relay.connecting = true;
                // The header goes first, as the guest reads it.
                relay.up = Some(std::mem::take(&mut relay.head));
                let spawned = std::thread::Builder::new()
                    .name("vsock leg".into())
                    .spawn(move || leg(key, &sock, &sender));
                if spawned.is_err() {
                    self.drop_relay(io, key);
                }
                return;
            };
            if let Some(chunk) = relay.up.take() {
                let end = chunk.is_empty();
                match legs.up.try_send(chunk) {
                    Ok(()) if end => {
                        relay.up_done = true;
                        return;
                    }
                    Ok(()) => {
                        if relay.end_next {
                            relay.end_next = false;
                            relay.up = Some(Vec::new());
                        }
                        continue;
                    }
                    Err(TrySendError::Full(chunk)) => {
                        relay.up = Some(chunk);
                        return;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        self.drop_relay(io, key);
                        return;
                    }
                }
            }
            match io.read_within(conn, stream, CHUNK) {
                Ok(Read::Bytes(b)) => relay.up = Some(b),
                Ok(Read::End(b)) => {
                    relay.end_next = !b.is_empty();
                    relay.up = Some(b);
                }
                Ok(Read::Blocked) => return,
                Ok(Read::Reset) | Err(_) => {
                    self.drop_relay(io, key);
                    return;
                }
            }
        }
    }

    /// The guest's bytes, on to the stream.
    fn down(&mut self, io: &mut Io<'_>, key: Key) {
        let (conn, stream) = key;
        let Some(relay) = self.relays.get_mut(&key) else {
            return;
        };
        if let Some((bytes, sent)) = &mut relay.down {
            while *sent < bytes.len() {
                match io.write(conn, stream, bytes.get(*sent..).unwrap_or_default()) {
                    Ok(0) => return,
                    Ok(n) => *sent = sent.saturating_add(n),
                    Err(_) => {
                        self.drop_relay(io, key);
                        return;
                    }
                }
            }
            relay.down = None;
            if let Some(legs) = &relay.legs {
                let _ = legs.permit.try_send(());
            }
        }
        if relay.down_end && !relay.finished {
            let _ = io.finish(conn, stream);
            relay.finished = true;
        }
        if relay.finished && relay.up_done {
            // Both ways done: its legs' threads end as their channels and socket close.
            self.relays.remove(&key);
        }
    }
}

/// A stream's vsock leg: connects to the guest's port through the VM's socket, starts the
/// writer, tells the driver, then reads what the guest says until it ends.
fn leg(key: Key, sock: &std::path::Path, sender: &Sender<Msg>) {
    let made = (|| -> Result<(Legs, VsockStream, Receiver<()>), String> {
        let socket = vsock_connect(sock, proto::VSOCK_PORT)?;
        let reader = socket.try_clone().map_err(|e| e.to_string())?;
        let writer = socket.try_clone().map_err(|e| e.to_string())?;
        let (up, chunks) = mpsc::sync_channel::<Vec<u8>>(1);
        let (permit, permits) = mpsc::sync_channel::<()>(1);
        let tell = sender.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("vsock writer".into())
            .spawn(move || write_leg(key, writer, &chunks, &tell))
            .map_err(|e| e.to_string())?;
        Ok((Legs { up, permit, socket }, reader, permits))
    })();
    let (legs, mut reader, permits) = match made {
        Ok(made) => made,
        Err(e) => {
            let _ = sender.send(Command::Handler(Msg::Up { key, legs: Err(e) }));
            return;
        }
    };
    if sender.send(Command::Handler(Msg::Up { key, legs: Ok(legs) })).is_err() {
        return;
    }
    let mut buf = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = sender.send(Command::Handler(Msg::GuestEnd { key }));
                return;
            }
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or_default().to_vec();
                if sender.send(Command::Handler(Msg::FromGuest { key, bytes })).is_err() {
                    return;
                }
                if permits.recv().is_err() {
                    return;
                }
            }
        }
    }
}

fn write_leg(key: Key, mut writer: VsockStream, chunks: &Receiver<Vec<u8>>, sender: &Sender<Msg>) {
    for chunk in chunks {
        if chunk.is_empty() {
            let _ = writer.shutdown(std::net::Shutdown::Write);
        } else if writer.write_all(&chunk).is_err() {
            return;
        }
        if sender.send(Command::Handler(Msg::Drained { key })).is_err() {
            return;
        }
    }
}

/// A stream to guest vsock port `port` through the VM's socket `sock`, once the VM says the
/// guest took it (`OK <host port>`, muxer.rs).
pub fn vsock_connect(sock: &std::path::Path, port: u32) -> Result<VsockStream, String> {
    let mut s = VsockStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    s.write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| format!("CONNECT: {e}"))?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        match s.read(&mut byte) {
            Ok(1) => line.extend_from_slice(&byte),
            _ => {
                return Err(format!(
                    "vsock port {port} refused after {:?}",
                    String::from_utf8_lossy(&line)
                ));
            }
        }
    }
    if !line.starts_with(b"OK ") {
        return Err(format!("vsock port {port}: {:?}", String::from_utf8_lossy(&line)));
    }
    Ok(s)
}

impl Handler for Endpoint {
    type Command = Msg;

    fn connected(&mut self, _io: &mut Io<'_>, _conn: ConnKey, peer: &[u8]) -> Result<(), Close> {
        if shards_remote::identity::fingerprint(peer) == self.controller {
            Ok(())
        } else {
            Err(Close {
                code: 1,
                why: "not the controller this endpoint was told",
            })
        }
    }

    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.relays.insert((conn, stream), Relay::default());
        // What came with the stream's first frame: quinn-proto tells of a stream the peer
        // opened (Opened) and not of the bytes that opened it (Readable).
        self.up(io, (conn, stream));
    }

    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if self.relays.contains_key(&(conn, stream)) {
            self.up(io, (conn, stream));
        } else {
            // A stream relayed and let go: its end is read, so QUIC lets it go too.
            let _ = io.read(conn, stream);
        }
    }

    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.down(io, (conn, stream));
    }

    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        self.drop_relay(io, (conn, stream));
    }

    fn lost(&mut self, io: &mut Io<'_>, conn: ConnKey, _why: &str, _code: Option<u64>) {
        let keys: Vec<Key> = self.relays.keys().filter(|(c, _)| *c == conn).copied().collect();
        for key in keys {
            self.drop_relay(io, key);
        }
    }

    fn command(&mut self, io: &mut Io<'_>, msg: Msg) {
        match msg {
            Msg::Up { key, legs } => match (self.relays.get_mut(&key), legs) {
                (Some(relay), Ok(legs)) => {
                    relay.legs = Some(legs);
                    relay.connecting = false;
                    self.up(io, key);
                }
                (Some(_), Err(_)) => self.drop_relay(io, key),
                (None, Ok(legs)) => legs.abandon(),
                (None, Err(_)) => {}
            },
            Msg::FromGuest { key, bytes } => {
                if let Some(relay) = self.relays.get_mut(&key) {
                    relay.down = Some((bytes, 0));
                    self.down(io, key);
                }
            }
            Msg::GuestEnd { key } => {
                if let Some(relay) = self.relays.get_mut(&key) {
                    relay.down_end = true;
                    self.down(io, key);
                }
            }
            Msg::Drained { key } => self.up(io, key),
        }
    }

    fn done(&self) -> bool {
        false
    }
}

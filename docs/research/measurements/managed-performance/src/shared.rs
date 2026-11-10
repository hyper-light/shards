//! Throwaway one-listener / one-connection adapter. All record parsing, cursor
//! validation, ownership checks and retained slots are the existing Service.
//! This valid-owner experiment is not a production refusal-isolation design.

use super::*;
use crate::managed::{self, CONTROL_BYTES, Ending, Input, Reason, Service, Shared, Slot, Subscription};
use hyper_quic::VarInt;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Condvar, Mutex};

pub(super) struct ChannelControl {
    shared: Arc<Shared>,
    sender: Sender<()>,
}
impl ChannelControl {
    pub(super) fn progress(&self) -> managed::Progress {
        managed::progress(&managed::lock(&self.shared).0)
    }
    pub(super) fn abort(&self) {
        {
            let mut state = managed::lock(&self.shared);
            let at = managed::progress(&state.0);
            state.0 = Slot::Aborted {
                accepted: at.accepted,
                consumed: at.consumed,
            };
            state.1 = Ending::Aborted;
        }
        self.shared.notify();
        // An already-full command handoff has an existing wake; the handler
        // checks all authoritative slots. Owner stop has a separate atomic flag.
        let _ = self.sender.send(Command::Handler(()));
    }
    pub(super) fn stop_owner(&self) {
        self.sender.request_stop();
    }
}

struct Pending {
    stream: StreamId,
    bytes: [u8; CONTROL_BYTES],
    len: usize,
}
struct Server {
    services: Vec<Service>,
    routes: Vec<(StreamId, usize)>,
    pending: Vec<Pending>,
}
impl Server {
    fn flush(&mut self, io: &mut Io<'_>) {
        for service in &mut self.services {
            let aborted = matches!(managed::lock(&service.shared).1, Ending::Aborted);
            if aborted {
                if let Subscription::Subscribed { conn, stream } = service.state {
                    io.stop(conn, stream);
                    io.reset(conn, stream);
                }
                service.state = Subscription::Closed;
            } else {
                service.flush(io);
            }
        }
    }
    fn refuse(io: &mut Io<'_>, conn: ConnKey, reason: Reason) {
        io.close(conn, reason as u32, "private comparison subscription refused");
    }
}
impl Handler for Server {
    type Command = ();
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> std::result::Result<(), Close> {
        for service in &mut self.services {
            service.connected(io, conn, peer)?;
        }
        Ok(())
    }
    fn opened(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if self.pending.len().saturating_add(self.routes.len()) >= self.services.len() {
            Self::refuse(io, conn, Reason::Scope);
            return;
        }
        self.pending.push(Pending {
            stream,
            bytes: [0; CONTROL_BYTES],
            len: 0,
        });
        self.readable(io, conn, stream);
    }
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some((_, index)) = self.routes.iter().find(|(id, _)| *id == stream) {
            if let Some(service) = self.services.get_mut(*index) {
                service.readable(io, conn, stream);
            }
            return;
        }
        let Some(position) = self.pending.iter().position(|p| p.stream == stream) else {
            Self::refuse(io, conn, Reason::Protocol);
            return;
        };
        let Some(pending) = self.pending.get_mut(position) else {
            return;
        };
        loop {
            let bytes = match io.read_within(conn, stream, CONTROL_BYTES.saturating_sub(pending.len)) {
                Ok(Read::Bytes(bytes)) => bytes,
                Ok(Read::Blocked) => return,
                _ => {
                    Self::refuse(io, conn, Reason::Protocol);
                    return;
                }
            };
            let Some(end) = pending.len.checked_add(bytes.len()) else {
                Self::refuse(io, conn, Reason::Protocol);
                return;
            };
            let Some(target) = pending.bytes.get_mut(pending.len..end) else {
                Self::refuse(io, conn, Reason::Protocol);
                return;
            };
            target.copy_from_slice(&bytes);
            pending.len = end;
            let offered = match managed::input(pending.bytes.get(..pending.len).unwrap_or_default()) {
                Ok(Some((Input::Subscribe(scope, _), _))) => scope,
                Ok(None) => continue,
                _ => {
                    Self::refuse(io, conn, Reason::Protocol);
                    return;
                }
            };
            let Some(index) = self.services.iter().position(|s| s.scope == offered) else {
                Self::refuse(io, conn, Reason::Scope);
                return;
            };
            if self.routes.iter().any(|(_, routed)| *routed == index) {
                Self::refuse(io, conn, Reason::Scope);
                return;
            }
            let pending = self.pending.remove(position);
            let Some(service) = self.services.get_mut(index) else {
                return;
            };
            service.control = pending.bytes;
            service.control_len = pending.len;
            if let Err(reason) = service.controls(io, conn, stream) {
                Self::refuse(io, conn, reason);
                return;
            }
            self.routes.push((stream, index));
            return;
        }
    }
    fn writable(&mut self, io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some((_, index)) = self.routes.iter().find(|(id, _)| *id == stream) {
            if let Some(service) = self.services.get_mut(*index) {
                service.stopped(io, conn, stream);
            }
        }
    }
    fn lost(&mut self, io: &mut Io<'_>, conn: ConnKey, why: &str, code: Option<u64>) {
        for service in &mut self.services {
            service.lost(io, conn, why, code);
        }
        self.routes.clear();
        self.pending.clear();
    }
    fn command(&mut self, io: &mut Io<'_>, (): ()) {
        self.flush(io);
    }
    fn done(&self) -> bool {
        false
    }
}

fn aggregate(admitted: Admitted, channels: usize, receive: usize) -> Result<hyper_quic::TransportConfig> {
    let mut transport = managed::transport(admitted, receive)?;
    transport
        .receive_window(VarInt::from_u64(u64::try_from(
            receive.checked_mul(channels).ok_or("receive overflow")?,
        )?)?)
        .send_window(
            admitted
                .transport_bytes
                .checked_mul(channels as u64)
                .ok_or("send overflow")?,
        )
        .max_concurrent_bidi_streams(VarInt::from_u64(channels as u64)?);
    Ok(transport)
}

pub(super) fn bind(
    live: &mut Vec<Live>,
    channels: usize,
    identity: &Identity,
    owner: &Identity,
    admitted: Admitted,
) -> Result<()> {
    let mut services = Vec::new();
    services.try_reserve_exact(channels)?;
    let mut routes = Vec::new();
    routes.try_reserve_exact(channels)?;
    let mut pending = Vec::new();
    pending.try_reserve_exact(channels)?;
    for index in 0..channels {
        services.push(Service {
            scope: Scope {
                run: RunId([0; 32]),
                generation: 1,
                member: u32::try_from(index / 2)?,
                channel: if index % 2 == 0 {
                    Channel::Stdout
                } else {
                    Channel::Stderr
                },
            },
            owner: owner.fingerprint(),
            state: Subscription::Listening,
            control: [0; CONTROL_BYTES],
            control_len: 0,
            shared: Arc::new(Shared {
                slot: Mutex::new((Slot::Waiting(0), Ending::Open)),
                changed: Condvar::new(),
                record_bytes: AtomicUsize::new(admitted.record_bytes),
                waker: Mutex::new(None),
            }),
        });
    }
    let mut config = crate::tls::owner_server(
        identity,
        owner.fingerprint(),
        aggregate(admitted, channels, CONTROL_BYTES)?,
        managed::ALPN,
    )?;
    config
        .max_incoming(1)
        .incoming_buffer_size(0)
        .incoming_buffer_size_total(0);
    let (mut driver, sender) = Driver::bind_bounded(
        "127.0.0.1:0".parse()?,
        Some(config),
        Server {
            services,
            routes,
            pending,
        },
        DriverLimits {
            connections: 1,
            command_slots: channels,
            queued_datagram_bytes: admitted
                .datagram_bytes
                .checked_mul(channels)
                .ok_or("queue overflow")?,
            handshake_patience: admitted.patience,
        },
    )?;
    let address = driver.local()?;
    if matches!(address.port(), 80 | 443) {
        return Err("excluded fixture port".into());
    }
    println!(
        "server_socket_allowance\tchannels={channels}\tbytes={}",
        driver
            .kernel_buffer_bytes()?
            .checked_add(sender.kernel_buffer_bytes()?)
            .ok_or("socket overflow")?
    );
    for service in &driver.handler_mut().services {
        live.push(Live {
            producer: Some(Producer {
                shared: Arc::clone(&service.shared),
                sender: sender.try_clone()?,
            }),
            control: OutputControl::Shared(ChannelControl {
                shared: Arc::clone(&service.shared),
                sender: sender.try_clone()?,
            }),
            server: None,
            client: None,
            sender: None,
            events: None,
            address,
            scope: service.scope,
        });
    }
    let handle = thread::Builder::new()
        .name("shared-server".into())
        .spawn(move || {
            driver.run()?;
            Ok(())
        })?;
    live.first_mut().ok_or("no channels")?.server = Some(handle);
    Ok(())
}

struct Clients {
    channels: Vec<Client>,
    config: Option<ClientConfig>,
    conn: Option<ConnKey>,
}
impl Handler for Clients {
    type Command = Ask;
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, peer: &[u8]) -> std::result::Result<(), Close> {
        for channel in &mut self.channels {
            channel.conn = Some(conn);
            channel.connected(io, conn, peer)?;
        }
        Ok(())
    }
    fn opened(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some(channel) = self.channels.iter_mut().find(|c| c.stream == Some(stream)) {
            channel.readable(io, conn, stream);
        }
    }
    fn writable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some(channel) = self.channels.iter_mut().find(|c| c.stream == Some(stream)) {
            channel.writable(io, conn, stream);
        }
    }
    fn stopped(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        if let Some(channel) = self.channels.iter_mut().find(|c| c.stream == Some(stream)) {
            channel.stopped(io, conn, stream);
        }
    }
    fn lost(&mut self, io: &mut Io<'_>, conn: ConnKey, why: &str, code: Option<u64>) {
        for channel in &mut self.channels {
            channel.lost(io, conn, why, code);
        }
    }
    fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
        match ask {
            Ask::Connect(to) => {
                if let Some(config) = self.config.take() {
                    match io.connect(to, config) {
                        Ok(conn) => self.conn = Some(conn),
                        Err(e) => {
                            for c in &mut self.channels {
                                c.fail(&e);
                            }
                        }
                    }
                }
            }
        }
    }
    fn done(&self) -> bool {
        self.channels.iter().all(|c| c.done)
    }
    fn due(&self) -> Option<Instant> {
        self.channels.iter().map(|c| c.deadline).min()
    }
    fn timeout(&mut self, io: &mut Io<'_>, now: Instant) {
        for c in &mut self.channels {
            c.timeout(io, now);
        }
    }
}
pub(super) fn connect(
    live: &mut [Live],
    owner: &Identity,
    identity: &Identity,
    admitted: Admitted,
    stall: bool,
) -> Result<()> {
    let count = live.len();
    let mut channels = Vec::new();
    channels.try_reserve_exact(count)?;
    for (index, entry) in live.iter_mut().enumerate() {
        let (event, events) = mpsc::sync_channel(1);
        channels.push(Client {
            config: None,
            scope: entry.scope,
            conn: None,
            stream: None,
            event,
            inbound: Vec::new(),
            pending: Vec::new(),
            written: 0,
            record_bytes: admitted.record_bytes,
            ack: !(stall && index == 0),
            done: false,
            deadline: Instant::now()
                .checked_add(admitted.patience)
                .ok_or("deadline overflow")?,
        });
        entry.events = Some(events);
    }
    let receive = admitted
        .record_bytes
        .checked_add(managed::DATA_HEAD)
        .ok_or("receive overflow")?;
    let config = crate::tls::client_with(
        owner,
        identity.fingerprint(),
        aggregate(admitted, count, receive)?,
        managed::ALPN,
    )?;
    let (mut driver, sender) = Driver::bind_bounded(
        "127.0.0.1:0".parse()?,
        None,
        Clients {
            channels,
            config: Some(config),
            conn: None,
        },
        DriverLimits {
            connections: 1,
            command_slots: count,
            queued_datagram_bytes: admitted
                .datagram_bytes
                .checked_mul(count)
                .ok_or("queue overflow")?,
            handshake_patience: admitted.patience,
        },
    )?;
    let entry = live.first_mut().ok_or("no channels")?;
    sender.send(Command::Handler(Ask::Connect(entry.address)))?;
    entry.client = Some(
        thread::Builder::new()
            .name("shared-client".into())
            .spawn(move || {
                driver.run()?;
                Ok(())
            })?,
    );
    entry.sender = Some(sender);
    Ok(())
}

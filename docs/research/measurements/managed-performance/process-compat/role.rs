//! Fixture-only roles; no installed executable or replacement protocol/security path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::indexing_slicing
)]

use hyper_quic::{ClientConfig, StreamId};
use shards_abi::run::managed::Channel;
use shards_remote::frame::{self, Tag};
use shards_remote::identity::Identity;
use shards_remote::managed::{self, Admitted, Control, Endpoint, Producer, RunId, Scope};
use shards_remote::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};
use std::error::Error;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
// Explicit bounded fixture inputs, never runtime authority or tuning policy.
const BYTES: usize = 1024;
const RECORDS: usize = 16;
const PATIENCE: Duration = Duration::from_secs(10);
fn admitted() -> Admitted {
    Admitted {
        record_bytes: BYTES,
        transport_bytes: (BYTES + managed::DATA_HEAD) as u64,
        handshake_bytes: 16384,
        datagram_bytes: managed::DATAGRAM_BYTES,
        patience: PATIENCE,
    }
}
fn scope(index: usize) -> Scope {
    Scope {
        run: RunId([7; 32]),
        generation: 1,
        member: (index / 2) as u32,
        channel: if index.is_multiple_of(2) {
            Channel::Stdout
        } else {
            Channel::Stderr
        },
    }
}
fn emit(text: impl std::fmt::Display) {
    println!("{text}");
    std::io::stdout().flush().unwrap();
}
fn handles() -> (usize, usize) {
    (
        std::fs::read_dir("/proc/self/fd").unwrap().count(),
        std::fs::read_dir("/proc/self/task").unwrap().count(),
    )
}
fn snapshot(phase: &str) -> Result<()> {
    if std::env::var_os("MANAGED_PROCESS_RESOURCES").is_none() {
        return Ok(());
    }
    for name in ["status", "smaps_rollup", "cgroup", "net/sockstat"] {
        for line in std::fs::read_to_string(Path::new("/proc/self").join(name))?.lines() {
            emit(format_args!("process_resource\t{phase}\t{name}\t{line}"));
        }
    }
    emit(format_args!("process_handles\t{phase}\t{:?}", handles()));
    Ok(())
}
#[derive(Debug)]
enum Notice {
    Subscribed,
    Finished(usize),
    OwnerEnd,
}
fn publish(p: Producer, control: &Control, notice: &mpsc::SyncSender<Notice>, index: usize) -> Result<()> {
    p.wait_capacity()?;
    notice.send(Notice::Subscribed)?;
    let payload: Vec<_> = (0..BYTES).map(|i| i as u8).collect();
    for _ in 0..RECORDS {
        p.submit(payload.clone()).map_err(|_| "fixture record refused")?;
        p.wait_capacity()?;
    }
    control.finish()?;
    notice.send(Notice::Finished(index))?;
    Ok(())
}
fn server(root: &Path, channels: usize, mode: &str) -> Result<()> {
    let identity = Identity::load_or_create(&root.join("server.key"))?;
    let owner = Identity::load_or_create(&root.join("owner.key"))?;
    let (notice, notices) = mpsc::sync_channel(
        channels
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or("notice overflow")?,
    );
    let mut controls = Vec::new();
    let mut drivers = Vec::new();
    let mut workers = Vec::new();
    let mut addresses = Vec::new();
    for index in 0..channels {
        let (endpoint, p, control) =
            Endpoint::bind(&identity, owner.fingerprint(), scope(index), admitted())?;
        addresses.push(endpoint.address.to_string());
        emit(format_args!(
            "process_socket_allowance\t{index}\t{}",
            endpoint
                .kernel_buffer_bytes()?
                .checked_add(control.kernel_buffer_bytes()?)
                .ok_or("socket sum")?
        ));
        let control = Arc::new(control);
        controls.push(Arc::clone(&control));
        drivers.push(
            thread::Builder::new()
                .name("fixture-server-driver".into())
                .spawn(move || endpoint.run())?,
        );
        let notice = notice.clone();
        workers.push(
            thread::Builder::new()
                .name("fixture-publisher".into())
                .spawn(move || publish(p, &control, &notice, index))?,
        );
    }
    std::fs::write(root.join("addresses"), addresses.join("\n"))?;
    snapshot("servers_bound")?;
    emit("managed_ready");
    let end = notice.clone();
    let input = thread::Builder::new()
        .name("fixture-owner-control".into())
        .spawn(move || {
            // EOF of this private fixture's inherited stdin is owner cancellation.
            let mut byte = [0];
            assert_eq!(std::io::stdin().read(&mut byte).unwrap(), 0);
            end.send(Notice::OwnerEnd).unwrap();
        })?;
    drop(notice);
    let mut subscribed = 0;
    let mut finished = 0;
    while let Ok(notice) = notices.recv_timeout(PATIENCE) {
        match notice {
            Notice::Subscribed => {
                subscribed += 1;
                if subscribed == channels {
                    snapshot("subscribed")?;
                }
            }
            Notice::Finished(index) => {
                finished += 1;
                emit(format_args!("managed_finished\t{index}"));
                if finished == channels {
                    snapshot("steady")?;
                }
            }
            Notice::OwnerEnd => break,
        }
    }
    for control in &controls {
        control.abort();
    }
    input.join().map_err(|_| "owner control panicked")?;
    for (index, worker) in workers.into_iter().enumerate() {
        let result = worker.join().map_err(|_| "publisher panicked")?;
        let progress = controls[index].progress();
        emit(format_args!(
            "managed_progress\t{index}\taccepted={}\tconsumed={}\tcompleted={}",
            progress.accepted,
            progress.consumed,
            result.is_ok()
        ));
        if mode == "normal" {
            result?;
            assert_eq!(progress.consumed, (BYTES * RECORDS) as u64);
        } else if mode.starts_with("wrong") {
            assert_eq!(progress.accepted, 0);
            assert_eq!(progress.consumed, 0);
            assert!(result.is_err());
        } else if index == 0 {
            assert_eq!(progress.accepted, BYTES as u64);
            assert_eq!(progress.consumed, 0);
            assert!(result.is_err());
        } else {
            result?;
            assert_eq!(progress.consumed, (BYTES * RECORDS) as u64);
        }
    }
    for driver in drivers {
        driver.join().map_err(|_| "server driver panicked")??;
    }
    drop(controls);
    Ok(())
}
#[derive(Debug)]
enum Outcome {
    Complete,
    Rejected,
    Cancelled,
    Failed(String),
}
#[derive(Debug)]
enum Ask {
    Connect(SocketAddr),
}
struct Observer {
    config: Option<ClientConfig>,
    conn: Option<ConnKey>,
    stream: Option<StreamId>,
    incoming: Vec<u8>,
    pending: Vec<u8>,
    written: usize,
    records: usize,
    index: usize,
    stall: bool,
    negative: bool,
    outcome: Option<Outcome>,
    deadline: Instant,
}
impl Observer {
    fn fail(&mut self, why: impl ToString) {
        self.outcome = Some(Outcome::Failed(why.to_string()));
    }
    fn flush(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        while let Some(part) = self.pending.get(self.written..).filter(|b| !b.is_empty()) {
            match io.write(conn, stream, part) {
                Ok(0) => return,
                Ok(n) => self.written += n,
                Err(e) => {
                    self.fail(e);
                    return;
                }
            }
        }
        self.pending.clear();
        self.written = 0;
    }
    fn queue(&mut self, io: &mut Io<'_>, data: &[u8]) {
        self.pending.extend_from_slice(data);
        self.flush(io);
    }
    fn receive(&mut self, io: &mut Io<'_>, data: &[u8]) -> Result<()> {
        self.incoming.extend_from_slice(data);
        while let Some((tag, body)) = frame::take(&mut self.incoming)? {
            if self.negative {
                return Err("negative pin received application payload".into());
            }
            let (offset, payload) = body.split_first_chunk::<8>().ok_or("record cursor")?;
            if tag != Tag::Stdout
                || u64::from_be_bytes(*offset) != (self.records * BYTES) as u64
                || payload.len() != BYTES
                || !payload.iter().enumerate().all(|(i, b)| *b == i as u8)
            {
                return Err("exact payload/cursor mismatch".into());
            }
            self.records += 1;
            if self.records == 1 {
                emit(format_args!("managed_record\t{}", self.index));
            }
            if !self.stall {
                self.queue(io, &managed::consumed((self.records * BYTES) as u64));
            }
        }
        Ok(())
    }
    fn disconnected(&mut self, why: &str, code: Option<u64>) {
        self.outcome = Some(if self.negative && self.records == 0 && code.is_none() {
            Outcome::Rejected
        } else if self.stall && self.records == 1 {
            Outcome::Cancelled
        } else {
            Outcome::Failed(why.to_owned())
        });
    }
}
impl Handler for Observer {
    type Command = Ask;
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _: &[u8]) -> std::result::Result<(), Close> {
        match io.open(conn) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.queue(io, &managed::subscribe(scope(self.index), 0));
                Ok(())
            }
            Err(e) => {
                self.fail(e);
                Err(Close {
                    code: 1,
                    why: "fixture open",
                })
            }
        }
    }
    fn opened(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {}
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let (bytes, end) = match io.read_within(conn, stream, BYTES + managed::DATA_HEAD) {
                Ok(Read::Bytes(b)) => (b, false),
                Ok(Read::End(b)) => (b, true),
                Ok(Read::Blocked) => return,
                other => {
                    self.fail(format!("read {other:?}"));
                    return;
                }
            };
            if let Err(e) = self.receive(io, &bytes) {
                self.fail(e);
                return;
            }
            if end {
                self.outcome = Some(if !self.stall && !self.negative && self.records == RECORDS {
                    Outcome::Complete
                } else {
                    Outcome::Failed("unexpected FIN".into())
                });
                return;
            }
        }
    }
    fn writable(&mut self, io: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, _: &mut Io<'_>, _: ConnKey, _: StreamId) {
        self.disconnected("stream stopped", None);
    }
    fn lost(&mut self, _: &mut Io<'_>, _: ConnKey, why: &str, code: Option<u64>) {
        emit(format_args!("managed_lost\t{}\tcode={code:?}\t{why}", self.index));
        self.disconnected(why, code);
    }
    fn command(&mut self, io: &mut Io<'_>, Ask::Connect(to): Ask) {
        let config = self.config.take().unwrap();
        match io.connect(to, config) {
            Ok(conn) => self.conn = Some(conn),
            Err(e) => self.fail(e),
        }
    }
    fn done(&self) -> bool {
        self.outcome.is_some()
    }
    fn due(&self) -> Option<Instant> {
        Some(self.deadline)
    }
    fn timeout(&mut self, _: &mut Io<'_>, _: Instant) {
        self.fail("fixture deadline, not a pin refusal");
    }
}
fn observer(root: &Path, mode: &str) -> Result<()> {
    let key = if mode == "wrong_client" {
        "intruder.key"
    } else {
        "owner.key"
    };
    let identity = Identity::load_or_create(&root.join(key))?;
    let peer = Identity::load_or_create(&root.join(if mode == "wrong_server" {
        "other-server.key"
    } else {
        "server.key"
    }))?;
    let mut drivers = Vec::new();
    let mut senders: Vec<Sender<Ask>> = Vec::new();
    for (index, line) in std::fs::read_to_string(root.join("addresses"))?
        .lines()
        .enumerate()
    {
        let observer = Observer {
            config: Some(managed::client_config(&identity, peer.fingerprint(), admitted())?),
            conn: None,
            stream: None,
            incoming: Vec::new(),
            pending: Vec::new(),
            written: 0,
            records: 0,
            index,
            stall: mode == "stall" && index == 0,
            negative: mode.starts_with("wrong"),
            outcome: None,
            deadline: Instant::now().checked_add(PATIENCE).ok_or("deadline")?,
        };
        let (mut driver, sender) = Driver::bind_bounded(
            "127.0.0.1:0".parse()?,
            None,
            observer,
            DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: managed::DATAGRAM_BYTES,
                handshake_patience: PATIENCE,
            },
        )?;
        sender.send(Command::Handler(Ask::Connect(line.parse()?)))?;
        senders.push(sender);
        drivers.push(
            thread::Builder::new()
                .name("fixture-client-driver".into())
                .spawn(move || -> Result<()> {
                    driver.run()?;
                    let handler = driver.handler_mut();
                    let outcome = handler.outcome.take().ok_or("missing client outcome")?;
                    emit(format_args!(
                        "managed_observer_done\t{}\t{:?}\trecords={}",
                        handler.index, outcome, handler.records
                    ));
                    match outcome {
                        Outcome::Complete | Outcome::Rejected | Outcome::Cancelled => Ok(()),
                        Outcome::Failed(why) => Err(why.into()),
                    }
                })?,
        );
    }
    snapshot("clients_bound")?;
    for driver in drivers {
        driver.join().map_err(|_| "client driver panicked")??;
    }
    drop(senders);
    snapshot("clients_complete")?;
    Ok(())
}
pub(super) fn run() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("MANAGED_PROCESS_ROOT").ok_or("fixture root")?);
    let role = std::env::var("MANAGED_PROCESS_ROLE")?;
    let mode = std::env::var("MANAGED_PROCESS_MODE")?;
    let channels: usize = std::env::var("MANAGED_PROCESS_CHANNELS")?.parse()?;
    if ![2, 4].contains(&channels)
        || !["normal", "stall", "wrong_server", "wrong_client"].contains(&mode.as_str())
    {
        return Err("fixture inputs".into());
    }
    let before = handles();
    snapshot("initial")?;
    if role == "server" {
        server(&root, channels, &mode)?;
    } else if role == "client" {
        observer(&root, &mode)?;
    } else {
        return Err("fixture role".into());
    }
    snapshot("closed")?;
    let after = handles();
    assert_eq!(before, after);
    emit(format_args!(
        "managed_process_return\t{role}\tbefore={before:?}\tafter={after:?}"
    ));
    Ok(())
}

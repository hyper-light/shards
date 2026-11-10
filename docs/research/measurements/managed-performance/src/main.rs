//! Private native measurement harness; no installed product or delegated authority.

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hyper_quic::{ClientConfig, StreamId};
use shards_abi::run::managed::Channel;
use shards_remote::frame::{self, Tag};
use shards_remote::identity::Identity;
use shards_remote::managed::{self, Admitted, Control, Endpoint, Producer, RunId, Scope};
use shards_remote::net::{Close, Command, ConnKey, Driver, DriverLimits, Handler, Io, Read, Sender};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

struct Count;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);

fn allocated(bytes: usize) {
    ALLOCS.fetch_add(1, Relaxed);
    TOTAL.fetch_add(bytes as u64, Relaxed);
    let live = LIVE.fetch_add(bytes as u64, Relaxed).saturating_add(bytes as u64);
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: every operation delegates the original pointer/layout to System. The
// optional observation uses only atomics, allocates nothing and never dereferences.
unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREES.fetch_add(1, Relaxed);
        LIVE.fetch_sub(layout.size() as u64, Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            REALLOCS.fetch_add(1, Relaxed);
            TOTAL.fetch_add(size as u64, Relaxed);
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
            let live = LIVE.fetch_add(size as u64, Relaxed).saturating_add(size as u64);
            PEAK.fetch_max(live, Relaxed);
        }
        next
    }
}

#[cfg(feature = "count-alloc")]
#[global_allocator]
static ALLOCATOR: Count = Count;

#[derive(Clone, Copy)]
struct Counts {
    alloc: u64,
    realloc: u64,
    free: u64,
    live: u64,
    total: u64,
    peak: u64,
}
fn counts() -> Counts {
    Counts {
        alloc: ALLOCS.load(Relaxed),
        realloc: REALLOCS.load(Relaxed),
        free: FREES.load(Relaxed),
        live: LIVE.load(Relaxed),
        total: TOTAL.load(Relaxed),
        peak: PEAK.load(Relaxed),
    }
}
fn phase(name: &str, before: Counts, elapsed: Duration) -> Result<()> {
    let end = counts();
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the correctly sized output on success.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let usage = unsafe { usage.assume_init() };
    println!(
        "phase\t{name}\twall_ns={}\talloc={}\trealloc={}\tfree={}\tallocated_bytes={}\tlive_bytes={}\tlive_delta={}\tpeak_bytes={}\tminor_faults={}\tmajor_faults={}\tuser_us={}\tsystem_us={}",
        elapsed.as_nanos(),
        end.alloc - before.alloc,
        end.realloc - before.realloc,
        end.free - before.free,
        end.total - before.total,
        end.live,
        i128::from(end.live) - i128::from(before.live),
        end.peak,
        usage.ru_minflt,
        usage.ru_majflt,
        usage.ru_utime.tv_sec * 1_000_000 + usage.ru_utime.tv_usec,
        usage.ru_stime.tv_sec * 1_000_000 + usage.ru_stime.tv_usec
    );
    #[cfg(target_os = "linux")]
    {
        for path in [
            "/proc/self/status",
            "/proc/self/smaps_rollup",
            "/proc/self/cgroup",
            "/proc/self/net/sockstat",
            "/proc/loadavg",
            "/proc/pressure/cpu",
        ] {
            let bytes = std::fs::read_to_string(path)?;
            for line in bytes.lines() {
                println!("resource\t{name}\t{path}\t{line}");
            }
        }
        println!(
            "resource\t{name}\tfds\t{}",
            std::fs::read_dir("/proc/self/fd")?.count()
        );
        println!(
            "resource\t{name}\ttasks\t{}",
            std::fs::read_dir("/proc/self/task")?.count()
        );
        let cgroup = std::fs::read_to_string("/proc/self/cgroup")?;
        if let Some(group) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) {
            let directory = std::path::Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
            for file in ["memory.current", "memory.peak", "memory.stat", "cpu.stat"] {
                if let Ok(bytes) = std::fs::read_to_string(directory.join(file)) {
                    for line in bytes.lines() {
                        println!("cgroup\t{name}\t{file}\t{line}");
                    }
                }
            }
        }
    }
    std::io::stdout().flush()?;
    Ok(())
}

#[derive(Debug)]
enum Event {
    Record(u64, Instant),
    Lost(Option<u64>),
    Failure(String),
}
#[derive(Debug)]
enum Ask {
    Connect(SocketAddr),
    Subscribe,
    Close,
}

struct Client {
    config: Option<ClientConfig>,
    scope: Scope,
    conn: Option<ConnKey>,
    stream: Option<StreamId>,
    event: mpsc::SyncSender<Event>,
    inbound: Vec<u8>,
    pending: Vec<u8>,
    written: usize,
    record_bytes: usize,
    ack: bool,
    done: bool,
    deadline: Instant,
}
impl Client {
    fn fail(&mut self, error: impl ToString) {
        let _ = self.event.try_send(Event::Failure(error.to_string()));
        self.done = true;
    }
    fn flush(&mut self, io: &mut Io<'_>) {
        let (Some(conn), Some(stream)) = (self.conn, self.stream) else {
            return;
        };
        while let Some(part) = self.pending.get(self.written..).filter(|b| !b.is_empty()) {
            match io.write(conn, stream, part) {
                Ok(0) => return,
                Ok(n) => self.written = self.written.saturating_add(n),
                Err(e) => {
                    self.fail(e);
                    return;
                }
            }
        }
        self.pending.clear();
        self.written = 0;
    }
    fn queue(&mut self, io: &mut Io<'_>, bytes: &[u8]) {
        if let Err(e) = self.pending.try_reserve_exact(bytes.len()) {
            self.fail(e);
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.flush(io);
    }
}
impl Handler for Client {
    type Command = Ask;
    fn connected(&mut self, io: &mut Io<'_>, conn: ConnKey, _peer: &[u8]) -> std::result::Result<(), Close> {
        match io.open(conn) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.queue(io, &managed::subscribe(self.scope, 0));
                Ok(())
            }
            Err(e) => {
                self.fail(e);
                Err(Close {
                    code: 1,
                    why: "measurement stream failed",
                })
            }
        }
    }
    fn opened(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {}
    fn readable(&mut self, io: &mut Io<'_>, conn: ConnKey, stream: StreamId) {
        loop {
            let read = io.read_within(conn, stream, self.record_bytes.saturating_add(managed::DATA_HEAD));
            let (bytes, end) = match read {
                Ok(Read::Bytes(bytes)) => (bytes, false),
                Ok(Read::End(bytes)) => (bytes, true),
                Ok(Read::Blocked) => break,
                other => {
                    self.fail(format!("read: {other:?}"));
                    return;
                }
            };
            if let Err(e) = self.inbound.try_reserve_exact(bytes.len()) {
                self.fail(e);
                return;
            }
            self.inbound.extend_from_slice(&bytes);
            loop {
                let (tag, bytes) = match frame::take(&mut self.inbound) {
                    Ok(Some(pair)) => pair,
                    Ok(None) => break,
                    Err(e) => {
                        self.fail(e);
                        return;
                    }
                };
                let Some((offset, payload)) = bytes.split_first_chunk::<8>() else {
                    self.fail("missing cursor");
                    return;
                };
                if tag != Tag::Stdout
                    || payload.len() != self.record_bytes
                    || !payload.iter().enumerate().all(|(i, b)| *b == i as u8)
                {
                    self.fail("synthetic byte mismatch");
                    return;
                }
                let offset = u64::from_be_bytes(*offset);
                let Some(next) = offset.checked_add(payload.len() as u64) else {
                    self.fail("cursor overflow");
                    return;
                };
                if self.event.try_send(Event::Record(next, Instant::now())).is_err() {
                    self.fail("record event capacity exceeded");
                    return;
                }
                if self.ack {
                    self.queue(io, &managed::consumed(next));
                }
            }
            if end {
                break;
            }
        }
    }
    fn writable(&mut self, io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.flush(io);
    }
    fn stopped(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _stream: StreamId) {
        self.fail("stopped");
    }
    fn lost(&mut self, _io: &mut Io<'_>, _conn: ConnKey, _why: &str, code: Option<u64>) {
        let _ = self.event.try_send(Event::Lost(code));
        self.done = true;
    }
    fn command(&mut self, io: &mut Io<'_>, ask: Ask) {
        match ask {
            Ask::Connect(to) => {
                let Some(config) = self.config.take() else {
                    self.fail("no config");
                    return;
                };
                match io.connect(to, config) {
                    Ok(conn) => self.conn = Some(conn),
                    Err(e) => self.fail(e),
                }
            }
            Ask::Subscribe => {
                if let Some(conn) = self.conn {
                    let _ = self.connected(io, conn, &[]);
                }
            }
            Ask::Close => {
                if let Some(conn) = self.conn {
                    io.close(conn, 0, "measurement complete");
                }
                self.done = true;
            }
        }
    }
    fn done(&self) -> bool {
        self.done
    }
    fn due(&self) -> Option<Instant> {
        Some(self.deadline)
    }
    fn timeout(&mut self, _io: &mut Io<'_>, _now: Instant) {
        self.fail("measurement deadline");
    }
}

struct Live {
    producer: Option<Producer>,
    control: Control,
    server: Option<JoinHandle<Result<()>>>,
    client: Option<JoinHandle<Result<()>>>,
    sender: Option<Sender<Ask>>,
    events: Option<mpsc::Receiver<Event>>,
    address: SocketAddr,
    scope: Scope,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.control.abort();
        if let Some(sender) = &self.sender {
            sender.request_stop();
        }
    }
}
impl Live {
    fn client(&mut self, owner: &Identity, server: &Identity, admitted: Admitted, ack: bool) -> Result<()> {
        let (event, events) = mpsc::sync_channel(1);
        let handler = Client {
            config: Some(managed::client_config(owner, server.fingerprint(), admitted)?),
            scope: self.scope,
            conn: None,
            stream: None,
            event,
            inbound: Vec::new(),
            pending: Vec::new(),
            written: 0,
            record_bytes: admitted.record_bytes,
            ack,
            done: false,
            deadline: Instant::now()
                .checked_add(admitted.patience)
                .ok_or("deadline overflow")?,
        };
        let (mut driver, sender) = Driver::bind_bounded(
            "127.0.0.1:0".parse()?,
            None,
            handler,
            DriverLimits {
                connections: 1,
                command_slots: 1,
                queued_datagram_bytes: admitted.datagram_bytes,
                handshake_patience: admitted.patience,
            },
        )?;
        sender.send(Command::Handler(Ask::Connect(self.address)))?;
        self.client = Some(
            thread::Builder::new()
                .name("measured-client".into())
                .spawn(move || {
                    driver.run()?;
                    Ok(())
                })?,
        );
        self.sender = Some(sender);
        self.events = Some(events);
        Ok(())
    }
    fn end(&mut self) -> Result<()> {
        self.control.abort();
        if let Some(sender) = self.sender.take() {
            sender.request_stop();
        }
        for handle in [self.server.take(), self.client.take()].into_iter().flatten() {
            handle.join().map_err(|_| "driver panicked")??;
        }
        Ok(())
    }
}

fn worker(
    producer: Producer,
    events: mpsc::Receiver<Event>,
    records: usize,
    bytes: usize,
    patience: Duration,
) -> Result<Vec<(u128, u128)>> {
    let mut samples = Vec::new();
    samples.try_reserve_exact(records)?;
    for record in 0..records {
        producer.wait_capacity()?;
        let mut payload = Vec::new();
        payload.try_reserve_exact(bytes)?;
        payload.extend((0..bytes).map(|i| i as u8));
        let start = Instant::now();
        producer.submit(payload).map_err(|e| format!("submit {e:?}"))?;
        let received = match events.recv_timeout(patience)? {
            Event::Record(next, at)
                if next
                    == ((record + 1) as u64)
                        .checked_mul(bytes as u64)
                        .ok_or("cursor overflow")? =>
            {
                at
            }
            other => return Err(format!("unexpected event {other:?}").into()),
        };
        producer.wait_capacity()?;
        samples.push((
            received.duration_since(start).as_nanos(),
            start.elapsed().as_nanos(),
        ));
    }
    Ok(samples)
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        return Err(
            "usage: managed-performance TENANTS RECORDS BYTES PATIENCE_MS normal|stall endpoint".into(),
        );
    }
    let tenants: usize = args.get(1).ok_or("tenants")?.parse()?;
    let records: usize = args.get(2).ok_or("records")?.parse()?;
    let bytes: usize = args.get(3).ok_or("bytes")?.parse()?;
    let patience = Duration::from_millis(args.get(4).ok_or("patience")?.parse()?);
    let stall = args.get(5).is_some_and(|s| s == "stall");
    if tenants == 0
        || records == 0
        || bytes == 0
        || patience.is_zero()
        || args.get(6).is_none_or(|s| s != "endpoint")
    {
        return Err("invalid workload".into());
    }
    let channels = tenants.checked_mul(2).ok_or("channels overflow")?;
    let transport_bytes = bytes
        .checked_add(managed::DATA_HEAD)
        .ok_or("transport overflow")? as u64;
    // These are operator-selected fixture allowances, not production defaults or
    // entitlements. TLS fixture allowance is passed explicitly via the environment.
    let handshake_bytes = std::env::var("MANAGED_HANDSHAKE_BYTES")?.parse()?;
    let datagram_bytes = std::env::var("MANAGED_DATAGRAM_BYTES")?.parse()?;
    let admitted = Admitted {
        record_bytes: bytes,
        transport_bytes,
        handshake_bytes,
        datagram_bytes,
        patience,
    };
    println!(
        "config\ttenants={tenants}\tchannels={channels}\trecords={records}\tbytes={bytes}\tmode={}\tvariant=endpoint\tcount_alloc={}",
        if stall { "stall" } else { "normal" },
        cfg!(feature = "count-alloc")
    );
    let empty = counts();
    phase("initial", empty, Duration::ZERO)?;
    let start = Instant::now();
    let before = counts();
    let identity = Identity::generate()?;
    let owner = Identity::generate()?;
    let mut live = Vec::new();
    live.try_reserve_exact(channels)?;
    for index in 0..channels {
        let scope = Scope {
            run: RunId([0; 32]),
            generation: 1,
            member: u32::try_from(index / 2)?,
            channel: if index % 2 == 0 {
                Channel::Stdout
            } else {
                Channel::Stderr
            },
        };
        let (endpoint, producer, control) = Endpoint::bind(&identity, owner.fingerprint(), scope, admitted)?;
        let address = endpoint.address;
        let queues = endpoint
            .kernel_buffer_bytes()?
            .checked_add(control.kernel_buffer_bytes()?)
            .ok_or("socket accounting overflow")?;
        println!("server_socket_allowance\tchannel={index}\tbytes={queues}");
        let server = thread::Builder::new()
            .name("measured-server".into())
            .spawn(move || {
                endpoint.run()?;
                Ok(())
            })?;
        live.push(Live {
            producer: Some(producer),
            control,
            server: Some(server),
            client: None,
            sender: None,
            events: None,
            address,
            scope,
        });
    }
    phase("servers_bound", before, start.elapsed())?;
    let start = Instant::now();
    let before = counts();
    for (index, channel) in live.iter_mut().enumerate() {
        channel.client(&owner, &identity, admitted, !(stall && index == 0))?;
    }
    for channel in &live {
        channel.producer.as_ref().ok_or("producer")?.wait_capacity()?;
    }
    phase("tls_subscribed", before, start.elapsed())?;
    let start = Instant::now();
    let before = counts();
    let mut workers = Vec::new();
    workers.try_reserve_exact(channels)?;
    let mut blocked = None;
    for (index, channel) in live.iter_mut().enumerate() {
        let producer = channel.producer.take().ok_or("producer")?;
        let events = channel.events.take().ok_or("events")?;
        if stall && index == 0 {
            let mut payload = Vec::new();
            payload.try_reserve_exact(bytes)?;
            payload.extend((0..bytes).map(|i| i as u8));
            producer
                .submit(payload)
                .map_err(|e| format!("stall submit {e:?}"))?;
            match events.recv_timeout(patience)? {
                Event::Record(_, _) => {}
                other => return Err(format!("stall {other:?}").into()),
            }
            if producer.capacity() != 0 || channel.control.progress().consumed != 0 {
                return Err("unacknowledged channel acquired credit".into());
            }
            blocked = Some(
                thread::Builder::new()
                    .name("blocked-publisher".into())
                    .spawn(move || producer.wait_capacity())?,
            );
        } else {
            workers.push((
                index,
                thread::Builder::new()
                    .name("publisher".into())
                    .spawn(move || worker(producer, events, records, bytes, patience))?,
            ));
        }
    }
    let mut all = Vec::new();
    all.try_reserve_exact(workers.len())?;
    for (index, handle) in workers {
        all.push((index, handle.join().map_err(|_| "publisher panicked")??));
    }
    phase("workload", before, start.elapsed())?;
    for (index, samples) in all {
        for (record, (received, acked)) in samples.into_iter().enumerate() {
            println!("sample\tchannel={index}\trecord={record}\treceive_ns={received}\tapp_ack_ns={acked}");
        }
    }
    if let Some(blocked) = blocked {
        let channel = live.first().ok_or("stall channel")?;
        let progress = channel.control.progress();
        if progress.accepted != bytes as u64 || progress.consumed != 0 {
            return Err("stall progress changed".into());
        }
        let start = Instant::now();
        channel.control.abort();
        if blocked.join().map_err(|_| "blocked publisher panicked")?.is_ok() {
            return Err("abort returned credit".into());
        }
        println!(
            "cancel\twaiter_ns={}\taccepted={}\tconsumed={}\tneighbor_records={}",
            start.elapsed().as_nanos(),
            progress.accepted,
            progress.consumed,
            records
                .checked_mul(channels.saturating_sub(1))
                .ok_or("count overflow")?
        );
    }
    let start = Instant::now();
    let before = counts();
    for channel in &mut live {
        channel.end()?;
    }
    drop(live);
    drop(identity);
    drop(owner);
    phase("closed", before, start.elapsed())?;
    println!("result\tpass");
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("measurement failed: {error}");
        std::process::exit(1);
    }
}

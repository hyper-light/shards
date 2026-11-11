//! One framed audit fixture. The broker records are original synthetic codec inputs,
//! not observations of a tool/peer effect. Corpus and dispatch review precede execution.
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::devices::virtio::fs::{self, proto, server::Server};
use shards_abi::run::audit as wire;

type Result<T> = io::Result<T>;
type Snapshot = [Vec<u8>; 2];

#[derive(Clone, Copy)]
enum Case {
    Committed,
    ReservedClose,
}

// Journal's first lease is call0. The original codec input uses broker request1;
// neither is the FUSE frame correlation, which starts at1 independently.
const AUDIT_CALL: u64 = 0;
const BROKER_REQUEST: u64 = 1;
const WORK: Duration = Duration::from_secs(30);

fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.into()))
    }
}
fn field<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N]> {
    let end = at
        .checked_add(N)
        .ok_or_else(|| io::Error::other("field overflow"))?;
    bytes
        .get(at..end)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::other("short actual field"))
}
fn amount(n: usize) -> Result<u64> {
    u64::try_from(n).map_err(io::Error::other)
}
fn sum(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| io::Error::other("extent overflow"))
}
fn input(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other(name.to_owned()))?;
    require(
        path.is_absolute(),
        format!("{name} must be an explicit absolute path"),
    )?;
    Ok(path)
}

struct Trace {
    root: PathBuf,
    log: File,
}
impl Trace {
    fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir(&root)?;
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("events.log"))?;
        Ok(Self { root, log })
    }
    fn event(&mut self, event: impl std::fmt::Display) -> Result<()> {
        let result = writeln!(self.log, "{event}").and_then(|()| self.log.flush());
        if let Err(error) = &result {
            let _ = writeln!(io::stderr(), "audit fixture record failure: {event}; {error:?}");
        }
        result
    }
    fn save(&self, name: &str, bytes: &[u8]) -> Result<()> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(name))?
            .write_all(bytes)
    }
}

struct Records {
    intent: Vec<u8>,
    terminal: Vec<u8>,
    intent_max: u64,
    terminal_max: u64,
}
fn corpus_bytes(path: &Path) -> Result<Vec<u8>> {
    require(
        std::fs::symlink_metadata(path)?.file_type().is_file(),
        "corpus entry is not regular",
    )?;
    let file = File::open(path)?;
    // The hash-verified private corpus is trusted fixture input; every data-file
    // read is nevertheless bounded by the actual one-WRITE protocol maximum.
    let mut bytes = Vec::new();
    file.take(u64::from(proto::MAX_WRITE) + 1)
        .read_to_end(&mut bytes)?;
    require(
        bytes.len() <= proto::MAX_WRITE as usize,
        "corpus data exceeds one actual WRITE",
    )?;
    Ok(bytes)
}
fn records(root: &Path, member: u32) -> Result<Records> {
    let prefix = format!("member-{member}-Send");
    let intent = corpus_bytes(&root.join(format!("{prefix}.intent")))?;
    let terminal = corpus_bytes(&root.join(format!("{prefix}.terminal")))?;
    let intent_max = amount(corpus_bytes(&root.join(format!("{prefix}.intent-max")))?.len())?;
    let shape_prefix = format!("{prefix}.terminal-shape-");
    let mut shapes = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 corpus name"))?;
        if let Some(index) = name.strip_prefix(&shape_prefix) {
            shapes.push((
                index.parse::<usize>().map_err(io::Error::other)?,
                amount(corpus_bytes(&entry.path())?.len())?,
            ));
        }
    }
    shapes.sort_unstable();
    require(!shapes.is_empty(), "missing original terminal shapes")?;
    for (expected, (actual, _)) in shapes.iter().enumerate() {
        require(
            expected == *actual,
            "terminal shapes are not the original dense sequence",
        )?;
    }
    let terminal_max = shapes
        .iter()
        .map(|(_, n)| *n)
        .max()
        .ok_or_else(|| io::Error::other("no shapes"))?;
    require(
        !intent.is_empty() && !terminal.is_empty(),
        "empty ordinary codec record",
    )?;
    require(
        amount(intent.len())? <= intent_max && amount(terminal.len())? <= terminal_max,
        "record exceeds actual codec bound",
    )?;
    require(
        amount(intent.len())? < intent_max || amount(terminal.len())? < terminal_max,
        "no retained codec slack",
    )?;
    Ok(Records {
        intent,
        terminal,
        intent_max,
        terminal_max,
    })
}

struct Corpus {
    scopes: [wire::Scope; 2],
    records: [Records; 2],
    config: wire::ExportConfig,
    slot: u64,
    budget: u64,
}
impl Corpus {
    fn load(root: &Path, trace: &mut Trace) -> Result<Self> {
        // The reviewed external manifest check binds exact source, scope, Send,
        // request1 and shape inputs before any build/actor. No bytes are rewritten.
        trace.save(
            "original-corpus-inputs.json",
            &corpus_bytes(&root.join("inputs.json"))?,
        )?;
        let scopes = [1, 3].map(|member| {
            wire::Scope::decode_startup(&corpus_bytes(&root.join(format!("member-{member}.scope")))?)
                .map_err(|e| io::Error::other(format!("scope codec: {e:?}")))
        });
        let [first, second] = scopes;
        let scopes = [first?, second?];
        let [first, second] = &scopes;
        require(first.member == 1 && second.member == 3, "wrong stable members")?;
        let original = wire::ExportConfig::decode(&corpus_bytes(&root.join("export-config.bin"))?)
            .map_err(|e| io::Error::other(format!("original export codec: {e:?}")))?;
        require(original.scopes() == scopes, "original export scopes differ")?;
        let data = [records(root, first.member)?, records(root, second.member)?];
        let [selected, _] = &data;
        let slot = sum(
            sum(selected.intent_max, selected.terminal_max)?,
            wire::SLOT_OVERHEAD,
        )?;
        let headers = amount(wire::JOURNAL_HEADER_BYTES)?
            .checked_mul(amount(scopes.len())?)
            .ok_or_else(|| io::Error::other("header overflow"))?;
        let budget = sum(headers, slot)?;
        // Original all-eight-call allocation is provenance, never the selected
        // one-slot aggregate budget and never an independent budget per member.
        let storage = wire::StorageRequest::new(budget, wire::Retention::UntilOwnerDeletion)
            .map_err(|e| io::Error::other(format!("storage codec: {e:?}")))?;
        let config = wire::ExportConfig::new(storage, scopes.to_vec())
            .map_err(|e| io::Error::other(format!("export codec: {e:?}")))?;
        trace.event(format!("codec_input_only operation=Send broker_request={BROKER_REQUEST} audit_call={AUDIT_CALL} Imax={} Tmax={} slot={slot} aggregate_B={budget} original_all_calls_B={}", selected.intent_max, selected.terminal_max, original.storage().bytes()))?;
        trace.save(
            "selected-export-config.bin",
            &config
                .encode()
                .map_err(|e| io::Error::other(format!("export codec: {e:?}")))?,
        )?;
        Ok(Self {
            scopes,
            records: data,
            config,
            slot,
            budget,
        })
    }
}

fn rlimit() -> Result<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: writable exact rlimit for this process; no limit changes here.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((limit.rlim_cur, limit.rlim_max))
}
fn closed(fd: libc::c_int) -> bool {
    // SAFETY: query immediately after ownership drop, before any new open;
    // querying this former descriptor does not create/close any descriptor.
    (unsafe { libc::fcntl(fd, libc::F_GETFD) }) == -1
        && io::Error::last_os_error().raw_os_error() == Some(libc::EBADF)
}
fn snapshot(files: &[File; 2], trace: &mut Trace, name: &str, budget: u64) -> Result<Snapshot> {
    let mut result = [Vec::new(), Vec::new()];
    for (index, (file, bytes)) in files.iter().zip(&mut result).enumerate() {
        let metadata = file.metadata()?;
        trace.event(format!(
            "snapshot={name} member_index={index} dev={} ino={} nlink={} length={}",
            metadata.dev(),
            metadata.ino(),
            metadata.nlink(),
            metadata.len()
        ))?;
        bytes.resize(
            usize::try_from(metadata.len().min(sum(budget, 1)?)).map_err(io::Error::other)?,
            0,
        );
        file.read_exact_at(bytes, 0)?;
        trace.save(&format!("{name}.member-index-{index}.bin"), bytes)?;
        require(
            metadata.len() <= budget,
            "actual file exceeds aggregate B; bounded prefix retained",
        )?;
    }
    Ok(result)
}
fn changed(before: &Snapshot, after: &Snapshot, offset: u64, bytes: &[u8]) -> Result<()> {
    let mut expected = before.clone();
    let [first, _] = &mut expected;
    let start = usize::try_from(offset).map_err(io::Error::other)?;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| io::Error::other("patch overflow"))?;
    first
        .get_mut(start..end)
        .ok_or_else(|| io::Error::other("patch outside held file"))?
        .copy_from_slice(bytes);
    require(
        expected == *after,
        "unexpected actual bytes outside selected record/prefix",
    )
}

struct Wire<'a> {
    stream: fs::Stream,
    trace: &'a mut Trace,
    deadline: Instant,
    sent: u64,
}
impl Wire<'_> {
    fn exchange(&mut self, opcode: u32, node: u64, body: &[u8]) -> Result<Vec<u8>> {
        let left = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "original case deadline"))?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.set_write_timeout(Some(left))?;
        self.sent = sum(self.sent, 1)?;
        let size = proto::IN_HEADER_BYTES
            .checked_add(body.len())
            .ok_or_else(|| io::Error::other("request overflow"))?;
        require(size <= fs::MAX_FRAME, "request exceeds actual frame bound")?;
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::try_from(size).map_err(io::Error::other)?.to_le_bytes());
        frame.extend_from_slice(&opcode.to_le_bytes());
        frame.extend_from_slice(&self.sent.to_le_bytes());
        frame.extend_from_slice(&node.to_le_bytes());
        // SAFETY: native identity observations; these fields confer no Scope.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        frame.extend_from_slice(&uid.to_le_bytes());
        frame.extend_from_slice(&gid.to_le_bytes());
        frame.extend_from_slice(&std::process::id().to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(body);
        self.trace
            .save(&format!("{:02}.request.bin", self.sent), &frame)?;
        if let Err(error) = fs::write_frame(&mut self.stream, &frame) {
            let _ = self
                .trace
                .event(format!("request={} write_error={error:?}", self.sent));
            return Err(error);
        }
        self.trace.event(format!(
            "request={} framed_write_ok bytes={}; backend pending",
            self.sent,
            frame.len()
        ))?;
        let reply = match fs::read_frame(&mut self.stream) {
            Ok(Some(reply)) => reply,
            Ok(None) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "actual frame EOF")),
            Err(error) => {
                let _ = self
                    .trace
                    .event(format!("request={} read_error={error:?}", self.sent));
                return Err(error);
            }
        };
        self.trace.save(&format!("{:02}.reply.bin", self.sent), &reply)?;
        let errno = i32::from_le_bytes(field(&reply, 4)?);
        self.trace
            .event(format!("request={} actual_fuse_errno={errno}", self.sent))?;
        require(
            Instant::now() <= self.deadline,
            "actual response past original deadline",
        )?;
        require(
            u32::from_le_bytes(field(&reply, 0)?) as usize == reply.len(),
            "wrong reply length",
        )?;
        require(
            u64::from_le_bytes(field(&reply, 8)?) == self.sent,
            "wrong actual unique ID",
        )?;
        require(errno == 0, format!("actual FUSE errno {errno}"))?;
        Ok(reply
            .get(proto::OUT_HEADER_BYTES..)
            .ok_or_else(|| io::Error::other("short reply"))?
            .to_vec())
    }
    fn control(
        &mut self,
        node: u64,
        fh: u64,
        scope: wire::Scope,
        request: wire::Request,
    ) -> Result<wire::Reply> {
        let mut body = fh.to_le_bytes().to_vec();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&wire::IOCTL.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        let size = u32::try_from(wire::CONTROL_BYTES).map_err(io::Error::other)?;
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(
            &request
                .encode()
                .map_err(|e| io::Error::other(format!("request codec: {e:?}")))?,
        );
        let reply = self.exchange(proto::op::IOCTL, node, &body)?;
        require(
            reply.len() == 16 + wire::CONTROL_BYTES && reply.get(..16) == Some(&[0; 16]),
            "actual ioctl result shape",
        )?;
        let reply = wire::Reply::decode(
            reply
                .get(16..)
                .ok_or_else(|| io::Error::other("short audit reply"))?,
        )
        .map_err(|e| io::Error::other(format!("reply codec: {e:?}")))?;
        self.trace
            .event(format!("request={} actual_audit_reply={reply:?}", self.sent))?;
        require(reply.scope == scope, "actual reply escaped bound Scope")?;
        Ok(reply)
    }
    fn open(&mut self, scope: wire::Scope) -> Result<(u64, u64)> {
        let found = self.exchange(proto::op::LOOKUP, 1, &scope.member_name())?;
        require(found.len() == 128, "actual LOOKUP entry size")?;
        let node = u64::from_le_bytes(field(&found, 0)?);
        require(node != 0 && node != 1, "LOOKUP did not return file node")?;
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend_from_slice(&0u32.to_le_bytes());
        let opened = self.exchange(proto::op::OPEN, node, &body)?;
        require(opened.len() == 16, "actual OPEN result size")?;
        let fh = u64::from_le_bytes(field(&opened, 0)?);
        require(
            fh != 0
                && u32::from_le_bytes(field(&opened, 8)?) == 1
                && u32::from_le_bytes(field(&opened, 12)?) == 0,
            "actual OPEN fh/direct-IO/backing differs",
        )?;
        Ok((node, fh))
    }
    fn append(&mut self, node: u64, fh: u64, offset: u64, bytes: &[u8]) -> Result<()> {
        let mut body = fh.to_le_bytes().to_vec();
        body.extend_from_slice(&offset.to_le_bytes());
        body.extend_from_slice(
            &u32::try_from(bytes.len())
                .map_err(io::Error::other)?
                .to_le_bytes(),
        );
        body.extend_from_slice(&[0; 20]);
        body.extend_from_slice(bytes);
        let reply = self.exchange(proto::op::WRITE, node, &body)?;
        require(
            reply.len() == 8
                && u32::from_le_bytes(field(&reply, 0)?) as usize == bytes.len()
                && u32::from_le_bytes(field(&reply, 4)?) == 0,
            "actual WRITE accepted a different interval",
        )
    }
    fn release(&mut self, node: u64, fh: u64) -> Result<()> {
        let mut body = fh.to_le_bytes().to_vec();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        require(
            self.exchange(proto::op::RELEASE, node, &body)?.is_empty(),
            "extra RELEASE reply bytes",
        )
    }
}

fn empty(reply: wire::Reply, remaining: u64, cause: wire::Cause) -> Result<()> {
    require(
        reply.stage == wire::Stage::Empty
            && reply.cause == cause
            && reply.remaining == remaining
            && reply.call == 0
            && reply.intent == 0
            && reply.terminal == 0
            && reply.intent_offset == 0
            && reply.terminal_offset == 0
            && reply.accepted == 0
            && reply.native.is_none()
            && reply.failure.is_none(),
        "actual empty/refusal state differs",
    )
}

fn sequence(
    wire: &mut Wire<'_>,
    corpus: &Corpus,
    files: &[File; 2],
    initial: &Snapshot,
    case: Case,
) -> Result<()> {
    let [scope, other] = corpus.scopes;
    let [data, neighbor] = &corpus.records;
    let mut init = 7u32.to_le_bytes().to_vec();
    init.extend_from_slice(&45u32.to_le_bytes());
    init.extend_from_slice(&[0; 56]);
    let initialized = wire.exchange(proto::op::INIT, 0, &init)?;
    require(
        initialized.len() == 64
            && u32::from_le_bytes(field(&initialized, 0)?) == 7
            && u32::from_le_bytes(field(&initialized, 4)?) == 45
            && u32::from_le_bytes(field(&initialized, 20)?) == proto::MAX_WRITE
            && u16::from_le_bytes(field(&initialized, 28)?) == proto::MAX_PAGES
            && u32::from_le_bytes(field(&initialized, 32)?) == 0,
        "actual INIT contract differs",
    )?;
    let (node, fh) = wire.open(scope)?;
    let (other_node, other_fh) = wire.open(other)?;
    require(
        node != other_node && fh != other_fh,
        "members share actual node/handle",
    )?;
    empty(
        wire.control(node, fh, scope, wire::Request::Identify)?,
        corpus.slot,
        wire::Cause::None,
    )?;
    empty(
        wire.control(other_node, other_fh, other, wire::Request::Identify)?,
        corpus.slot,
        wire::Cause::None,
    )?;
    require(
        snapshot(files, wire.trace, "identified", corpus.budget)? == *initial,
        "identification changed held files",
    )?;
    let reserve = wire::Request::Reserve {
        call: AUDIT_CALL,
        intent: data.intent_max,
        terminal: data.terminal_max,
    };
    let reserved = wire.control(node, fh, scope, reserve)?;
    let header = amount(wire::JOURNAL_HEADER_BYTES)?;
    let number = amount(size_of::<u64>())?;
    let immutable = number
        .checked_mul(3)
        .ok_or_else(|| io::Error::other("slot fields overflow"))?;
    require(
        reserved.stage == wire::Stage::Reserved
            && reserved.cause == wire::Cause::None
            && reserved.call == AUDIT_CALL
            && reserved.intent == data.intent_max
            && reserved.terminal == data.terminal_max
            && reserved.remaining == 0
            && reserved.accepted == 0
            && reserved.native.is_none()
            && reserved.failure.is_none()
            && reserved.intent_offset == sum(sum(header, immutable)?, number)?
            && reserved.terminal_offset == sum(sum(reserved.intent_offset, data.intent_max)?, number)?,
        "actual reservation differs",
    )?;
    let mut current = snapshot(files, wire.trace, "reserved", corpus.budget)?;
    let [first, second] = &current;
    let [initial_first, initial_second] = initial;
    require(
        amount(first.len())? == sum(header, corpus.slot)?
            && second == initial_second
            && first.get(..wire::JOURNAL_HEADER_BYTES) == Some(initial_first.as_slice()),
        "reservation changed headers/neighbor/extent",
    )?;
    let at = wire::JOURNAL_HEADER_BYTES;
    require(
        u64::from_be_bytes(field(first, at)?) == AUDIT_CALL
            && u64::from_be_bytes(field(first, at + size_of::<u64>())?) == data.intent_max
            && u64::from_be_bytes(field(first, at + 2 * size_of::<u64>())?) == data.terminal_max,
        "actual immutable slot differs",
    )?;
    match case {
        Case::Committed => {}
        Case::ReservedClose => {
            return reserved_close_sequence(
                wire,
                corpus,
                files,
                (node, fh, other_node, other_fh),
                reserved,
                &current,
            );
        }
    }
    require(
        wire.control(node, fh, scope, reserve)? == reserved,
        "duplicate Reserve differs",
    )?;
    require(
        snapshot(files, wire.trace, "duplicate-reserve", corpus.budget)? == current,
        "duplicate Reserve changed files",
    )?;

    wire.append(node, fh, reserved.intent_offset, &data.intent)?;
    let appended = snapshot(files, wire.trace, "intent-written", corpus.budget)?;
    changed(&current, &appended, reserved.intent_offset, &data.intent)?;
    current = appended;
    require(
        wire.control(node, fh, scope, wire::Request::Inspect { call: AUDIT_CALL })?
            == wire::Reply {
                stage: wire::Stage::IntentWritten,
                accepted: amount(data.intent.len())?,
                ..reserved
            },
        "actual intent cursor differs",
    )?;
    let intent = wire.control(node, fh, scope, wire::Request::CommitIntent { call: AUDIT_CALL })?;
    require(
        intent
            == wire::Reply {
                stage: wire::Stage::IntentSynced,
                ..reserved
            },
        "actual intent commit differs",
    )?;
    let committed = snapshot(files, wire.trace, "intent-committed", corpus.budget)?;
    changed(
        &current,
        &committed,
        reserved
            .intent_offset
            .checked_sub(number)
            .ok_or_else(|| io::Error::other("intent prefix"))?,
        &amount(data.intent.len())?.to_be_bytes(),
    )?;
    current = committed;
    require(
        wire.control(node, fh, scope, wire::Request::CommitIntent { call: AUDIT_CALL })? == intent,
        "duplicate intent commit differs",
    )?;
    require(
        snapshot(files, wire.trace, "duplicate-intent", corpus.budget)? == current,
        "duplicate intent commit changed files",
    )?;

    wire.append(node, fh, reserved.terminal_offset, &data.terminal)?;
    let appended = snapshot(files, wire.trace, "terminal-written", corpus.budget)?;
    changed(&current, &appended, reserved.terminal_offset, &data.terminal)?;
    current = appended;
    require(
        wire.control(node, fh, scope, wire::Request::Inspect { call: AUDIT_CALL })?
            == wire::Reply {
                stage: wire::Stage::TerminalWritten,
                accepted: amount(data.terminal.len())?,
                ..reserved
            },
        "actual terminal cursor differs",
    )?;
    let terminal = wire.control(
        node,
        fh,
        scope,
        wire::Request::CommitTerminal { call: AUDIT_CALL },
    )?;
    require(
        terminal
            == wire::Reply {
                stage: wire::Stage::TerminalSynced,
                ..reserved
            },
        "actual terminal commit differs",
    )?;
    let committed = snapshot(files, wire.trace, "terminal-committed", corpus.budget)?;
    changed(
        &current,
        &committed,
        reserved
            .terminal_offset
            .checked_sub(number)
            .ok_or_else(|| io::Error::other("terminal prefix"))?,
        &amount(data.terminal.len())?.to_be_bytes(),
    )?;
    current = committed;
    require(
        wire.control(
            node,
            fh,
            scope,
            wire::Request::CommitTerminal { call: AUDIT_CALL },
        )? == terminal,
        "duplicate terminal commit differs",
    )?;
    require(
        snapshot(files, wire.trace, "duplicate-terminal", corpus.budget)? == current,
        "duplicate terminal commit changed files",
    )?;

    let closed = wire.control(node, fh, scope, wire::Request::Close { call: AUDIT_CALL })?;
    require(
        closed
            == wire::Reply {
                stage: wire::Stage::Closed,
                ..reserved
            },
        "actual Close differs",
    )?;
    require(
        snapshot(files, wire.trace, "closed", corpus.budget)? == current,
        "Close released/changed extents",
    )?;
    require(
        wire.control(node, fh, scope, wire::Request::Close { call: AUDIT_CALL })? == closed,
        "duplicate Close differs",
    )?;
    require(
        snapshot(files, wire.trace, "duplicate-close", corpus.budget)? == current,
        "duplicate Close changed files",
    )?;
    require(
        wire.control(node, fh, scope, wire::Request::Inspect { call: AUDIT_CALL })? == closed,
        "closed Inspect differs",
    )?;
    require(
        snapshot(files, wire.trace, "closed-inspect", corpus.budget)? == current,
        "Inspect changed closed file",
    )?;
    empty(
        wire.control(
            other_node,
            other_fh,
            other,
            wire::Request::Reserve {
                call: AUDIT_CALL,
                intent: neighbor.intent_max,
                terminal: neighbor.terminal_max,
            },
        )?,
        0,
        wire::Cause::Exhausted,
    )?;
    require(
        snapshot(files, wire.trace, "neighbor-refused", corpus.budget)? == current,
        "refusal changed files",
    )?;
    empty(
        wire.control(other_node, other_fh, other, wire::Request::Identify)?,
        0,
        wire::Cause::None,
    )?;
    wire.release(node, fh)?;
    wire.release(other_node, other_fh)?;
    require(wire.sent == 24, "did not execute exact24 requests")?;
    require(
        snapshot(files, wire.trace, "released", corpus.budget)? == current,
        "RELEASE changed retained extents",
    )?;
    let [first, second] = &current;
    require(
        sum(amount(first.len())?, amount(second.len())?)? == corpus.budget,
        "actual total retained lengths differ from aggregate B",
    )
}

fn reserved_close_sequence(
    wire: &mut Wire<'_>,
    corpus: &Corpus,
    files: &[File; 2],
    handles: (u64, u64, u64, u64),
    reserved: wire::Reply,
    retained: &Snapshot,
) -> Result<()> {
    let (node, fh, other_node, other_fh) = handles;
    let [scope, other] = corpus.scopes;
    let [data, neighbor] = &corpus.records;
    let closed = wire::Reply {
        stage: wire::Stage::Closed,
        ..reserved
    };
    let reserve = wire::Request::Reserve {
        call: AUDIT_CALL,
        intent: data.intent_max,
        terminal: data.terminal_max,
    };
    // These are seven selected ordinary controls, not recovery retries. No
    // append occurs: every result must retain the exact post-Reserve objects.
    for (name, request, cause) in [
        (
            "reserved-close",
            wire::Request::Close { call: AUDIT_CALL },
            wire::Cause::None,
        ),
        (
            "reserved-close-duplicate",
            wire::Request::Close { call: AUDIT_CALL },
            wire::Cause::None,
        ),
        (
            "reserved-close-inspect",
            wire::Request::Inspect { call: AUDIT_CALL },
            wire::Cause::None,
        ),
        ("closed-reserve-refused", reserve, wire::Cause::Closed),
        (
            "closed-intent-refused",
            wire::Request::CommitIntent { call: AUDIT_CALL },
            wire::Cause::Closed,
        ),
        (
            "closed-terminal-refused",
            wire::Request::CommitTerminal { call: AUDIT_CALL },
            wire::Cause::Closed,
        ),
        (
            "closed-final-inspect",
            wire::Request::Inspect { call: AUDIT_CALL },
            wire::Cause::None,
        ),
    ] {
        require(
            wire.control(node, fh, scope, request)? == wire::Reply { cause, ..closed },
            format!("actual {name} state/cause/cursors/accepted/credit differ"),
        )?;
        require(
            snapshot(files, wire.trace, name, corpus.budget)? == *retained,
            format!("actual {name} changed the retained slot or neighbor"),
        )?;
    }
    require(
        wire.sent == 15,
        "did not execute exact seven post-Reserve controls",
    )?;
    empty(
        wire.control(
            other_node,
            other_fh,
            other,
            wire::Request::Reserve {
                call: AUDIT_CALL,
                intent: neighbor.intent_max,
                terminal: neighbor.terminal_max,
            },
        )?,
        0,
        wire::Cause::Exhausted,
    )?;
    require(
        snapshot(files, wire.trace, "closed-neighbor-refused", corpus.budget)? == *retained,
        "closed member refunded extents or neighbor refusal changed files",
    )?;
    empty(
        wire.control(other_node, other_fh, other, wire::Request::Identify)?,
        0,
        wire::Cause::None,
    )?;
    require(
        snapshot(files, wire.trace, "closed-neighbor-identified", corpus.budget)? == *retained,
        "neighbor identification changed files",
    )?;
    wire.release(node, fh)?;
    wire.release(other_node, other_fh)?;
    require(wire.sent == 19, "did not execute exact19 requests")?;
    require(
        snapshot(files, wire.trace, "closed-reserved-released", corpus.budget)? == *retained,
        "RELEASE changed the retained uncommitted slot",
    )?;
    let [first, second] = retained;
    require(
        sum(amount(first.len())?, amount(second.len())?)? == corpus.budget,
        "actual retained lengths differ from the aggregate B after early Close",
    )
}

fn owned_case(corpus: &Corpus, root: &Path, trace: &mut Trace, deadline: Instant, case: Case) -> Result<()> {
    let directory = File::open(root)?;
    let root_fd = directory.as_raw_fd();
    let server = match Server::audited(directory.into(), &corpus.config, 0) {
        Ok(server) => server,
        Err(error) => {
            let _ = trace.event(format!("actual_constructor_refusal={:?}", error.status()));
            return Err(io::Error::other(format!("actual constructor: {error}")));
        }
    };
    let result = (|| {
        let ready = server
            .audit_readiness()
            .map_err(|e| io::Error::other(format!("readiness codec: {e:?}")))?;
        trace.event(format!("actual_readiness={ready:?}"))?;
        require(
            ready
                == wire::ExportStatus::Ready {
                    remaining: corpus.slot,
                    objects: 2,
                },
            "actual readiness differs",
        )?;
        let paths = corpus
            .scopes
            .map(|scope| root.join(format!("{:08x}", scope.member)));
        let [first_path, second_path] = &paths;
        let files = [File::open(first_path)?, File::open(second_path)?];
        let result = (|| {
            let initial = snapshot(&files, trace, "constructed", corpus.budget)?;
            for (bytes, scope) in initial.iter().zip(corpus.scopes) {
                require(
                    bytes.len() == wire::JOURNAL_HEADER_BYTES,
                    "initial file not exact header",
                )?;
                let header = wire::JournalHeader::decode(bytes)
                    .map_err(|e| io::Error::other(format!("journal header codec: {e:?}")))?;
                require(
                    header.scope == scope && header.storage == corpus.config.storage(),
                    "header does not bind same aggregate storage",
                )?;
            }
            let mut names = std::fs::read_dir(root)?
                .map(|entry| entry.map(|e| e.file_name()))
                .collect::<Result<Vec<_>>>()?;
            names.sort();
            require(
                names
                    == [
                        std::ffi::OsString::from("00000001"),
                        std::ffi::OsString::from("00000003"),
                    ],
                "files-only gap or extra journal materialized",
            )?;
            let (client, served) = fs::Stream::pair()?;
            let client_fd = client.as_raw_fd();
            let server_fd = served.as_raw_fd();
            std::thread::scope(|scope| -> Result<()> {
                let worker = std::thread::Builder::new()
                    .name("audit-wire-fixture".into())
                    .spawn_scoped(scope, || fs::answer(&server, served))?;
                let mut wire = Wire {
                    stream: client,
                    trace,
                    deadline,
                    sent: 0,
                };
                let result = sequence(&mut wire, corpus, &files, &initial, case);
                let _ = wire.trace.event(format!(
                    "sequence_result={result:?} attempted_requests={}; later requests unreached on failure",
                    wire.sent
                ));
                shards_ipc::end(&wire.stream);
                drop(wire.stream);
                let client_closed = closed(client_fd);
                let joined = worker.join();
                let server_closed = closed(server_fd);
                wire.trace.event(format!("owned_end=true client_EBADF={client_closed} answer_join={joined:?} server_EBADF={server_closed}"))?;
                let joined = joined.map_err(|_| io::Error::other("answer thread panicked"))?;
                let cleanup =
                    require(client_closed && server_closed, "owned streams did not close").and(joined);
                if result.is_err() {
                    let captured = snapshot(&files, wire.trace, "failure-final", corpus.budget);
                    let _ = wire.trace.event(format!(
                        "failure_snapshot_result={:?}",
                        captured.as_ref().map(|_| ())
                    ));
                }
                result.and(cleanup)
            })
        })();
        for (file, path) in files.iter().zip(&paths) {
            let actual = file.metadata()?;
            let named = std::fs::metadata(path)?;
            trace.event(format!(
                "held_object_final path={} dev={} ino={} nlink={}",
                path.display(),
                actual.dev(),
                actual.ino(),
                actual.nlink()
            ))?;
            require(
                (actual.dev(), actual.ino(), actual.nlink()) == (named.dev(), named.ino(), 1),
                "held file identity/name/link changed",
            )?;
        }
        for file in files {
            let fd = file.as_raw_fd();
            drop(file);
            let was_closed = closed(fd);
            trace.event(format!("witness_fd={fd} EBADF={was_closed}"))?;
            require(was_closed, "owned witness did not close")?;
        }
        result
    })();
    drop(server);
    let root_closed = closed(root_fd);
    let recorded = trace.event(format!("server_dropped=true root_fd_EBADF={root_closed}; private writer/kernel population not independently counted"));
    result
        .and(recorded)
        .and(require(root_closed, "owned root descriptor did not close"))
}

fn run(case: Case) -> Result<()> {
    let began = Instant::now();
    let deadline = began
        .checked_add(WORK)
        .ok_or_else(|| io::Error::other("case deadline overflow"))?;
    let mut trace = Trace::new(input("SHARDS_AUDIT_WIRE_RECORDS")?)?;
    let corpus = Corpus::load(&input("SHARDS_AUDIT_WIRE_CORPUS")?, &mut trace)?;
    let temporary_parent = input("TMPDIR")?;
    require(
        std::fs::read_dir(&temporary_parent)?.next().is_none(),
        "TMPDIR must be new empty private parent",
    )?;
    let before = rlimit()?;
    trace.event(format!("before_tempdir_rlimit={before:?}"))?;
    let directory = shards_testdir::TempDir::new("audit-wire")?;
    trace.event(format!(
        "after_tempdir_rlimit={:?} actual_root={}",
        rlimit()?,
        directory.display()
    ))?;
    let result = owned_case(&corpus, &directory, &mut trace, deadline, case);
    let recorded = trace.event(format!("owned_case_result={result:?}"));
    let mut result = result.and(recorded);
    // Salvage paths only when setup failed before a held witness existed. These
    // bytes are not relabelled held-object evidence or a successful deletion.
    for scope in corpus.scopes {
        let path = directory.join(format!("{:08x}", scope.member));
        let captured = match corpus_bytes(&path) {
            Ok(bytes) => trace.save(&format!("path-salvage-member-{}.bin", scope.member), &bytes),
            Err(error) => {
                let _ = trace.event(format!("path_salvage_member={} error={error:?}", scope.member));
                Err(error)
            }
        };
        if let Err(error) = &captured {
            let _ = trace.event(format!(
                "path_salvage_member={} record_error={error:?}",
                scope.member
            ));
        }
        result = result.and(captured);
    }
    let actual_root = directory.path().to_path_buf();
    drop(directory);
    let absent = matches!(std::fs::symlink_metadata(&actual_root), Err(error) if error.kind() == io::ErrorKind::NotFound);
    let recorded = trace.event(format!(
        "actual_root_absent_after_owner_drop={absent} elapsed={:?}",
        began.elapsed()
    ));
    result
        .and(recorded)
        .and(require(absent, "owned root remains after deletion"))
}

#[test]
#[ignore = "requires ROOT-reviewed corpus/source and one admitted private native dispatch"]
fn full_fuse_frames_share_one_retained_audit_budget() {
    let outcome = run(Case::Committed);
    assert!(outcome.is_ok(), "actual native audit fixture: {outcome:?}");
}

#[test]
#[ignore = "requires separate ROOT source and one-case dispatch approval"]
fn reserved_close_retains_extent_and_refuses_reopening() {
    let outcome = run(Case::ReservedClose);
    assert!(
        outcome.is_ok(),
        "actual native reserved Close fixture: {outcome:?}"
    );
}

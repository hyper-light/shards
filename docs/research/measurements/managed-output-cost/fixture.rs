//! Coordinator-executed real-VM fixture. Implementer checks never execute this test.
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

mod parse;

struct Inputs {
    members: u32,
    records: PathBuf,
    patience: Duration,
}

impl Inputs {
    fn supplied() -> Self {
        let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("supply {name}"));
        let members = required("SHARDS_OUTPUT_COST_MEMBERS").parse::<u32>().unwrap();
        assert!(
            members.checked_add(1).is_some(),
            "files-only declaration and members must fit u32"
        );
        assert!(
            usize::try_from(members).unwrap().checked_mul(2).is_some(),
            "channel count must fit usize"
        );
        let patience = Duration::from_millis(required("SHARDS_OUTPUT_COST_DEADLINE_MS").parse().unwrap());
        assert!(
            !patience.is_zero(),
            "explicit finite positive phase deadline required"
        );
        assert!(
            Instant::now().checked_add(patience).is_some(),
            "representable phase deadline"
        );
        let records = PathBuf::from(required("SHARDS_OUTPUT_COST_RECORDS"));
        assert!(records.is_absolute(), "records must be an absolute new directory");
        std::fs::create_dir(&records).unwrap();
        Self {
            members,
            records,
            patience,
        }
    }

    fn write(&self, name: &str, value: &Value) {
        std::fs::write(self.records.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }
}

struct Member {
    id: u32,
    label: String,
    token: String,
}

enum Ready {
    NoManagedMembers {
        run: String,
    },
    OwnerChannels {
        run: String,
        endpoints: usize,
        unique_addresses: usize,
    },
}

impl Ready {
    fn record(&self) -> Value {
        match self {
            Self::NoManagedMembers { run } => {
                json!({"state":"NoManagedMembers","run":run,"endpoints":0,"proof":"detached run + main exact progress; managed-output record absent"})
            }
            Self::OwnerChannels {
                run,
                endpoints,
                unique_addresses,
            } => {
                json!({"state":"OwnerChannelsReady","run":run,"endpoints":endpoints,"unique_addresses":unique_addresses,"proof":"each scoped channel exact bytes received; all except member1 stdout ACK sent; main exact progress"})
            }
        }
    }
}

fn left(until: Instant) -> Duration {
    until.saturating_duration_since(Instant::now())
}

fn save_command(inputs: &Inputs, name: &str, result: &common::Run) {
    inputs.write(
        name,
        &json!({"status":result.status,"stdout":result.stdout,"stderr":result.stderr}),
    );
}

fn start(fixture: &mut Fixture, inputs: &Inputs, until: Instant) -> (Ready, Vec<Member>) {
    let (image, _) = common::served();
    let files = fixture.context.join("files");
    std::fs::create_dir(&files).unwrap();
    std::fs::write(files.join("asset"), b"files-only").unwrap();
    let mut agentfile = format!("FROM {image}\nAGENT assets FROM ./files\n");
    let mut members = Vec::new();
    // managed_plan assigns all AGENT IDs before HARNESS IDs, not source-line order.
    let agents = inputs.members.div_ceil(2);
    for ordinal in 0..inputs.members {
        let id = ordinal.checked_add(1).unwrap();
        let token = format!("member{id}");
        let kind = if ordinal < agents { "agent" } else { "harness" };
        let directory = fixture.context.join(&token);
        std::fs::create_dir(&directory).unwrap();
        common::program_copy(common::test_guest(), &directory.join("member"));
        std::fs::write(
            directory.join(format!("{kind}.json")),
            serde_json::to_vec(&json!({"entrypoint":"member","options":["managed-capture",token]})).unwrap(),
        )
        .unwrap();
        agentfile.push_str(&format!("{} {token} FROM ./{token}\n", kind.to_ascii_uppercase()));
        members.push(Member {
            id,
            label: format!("{kind} {token}"),
            token,
        });
    }
    std::fs::write(fixture.context.join("Agentfile"), &agentfile).unwrap();
    std::fs::write(inputs.records.join("Agentfile"), agentfile).unwrap();
    let built = fixture.command(
        &[
            "build",
            "image",
            "-t",
            "owner-output:1",
            fixture.context.to_str().unwrap(),
        ],
        left(until),
    );
    save_command(inputs, "build.json", &built);
    assert_eq!(built.status, Some(0), "{built}");
    let started = fixture.command(
        &[
            "run",
            "vm",
            "-d",
            "--name",
            "owner-output",
            "owner-output:1",
            "sleep",
        ],
        left(until),
    );
    save_command(inputs, "start.json", &started);
    assert_eq!(started.status, Some(0), "{started}");
    let run = started.stdout.trim().to_owned();
    assert!(
        !run.is_empty() && run.bytes().all(|b| b.is_ascii_hexdigit()),
        "run identity"
    );
    let record = fixture.home.join("containers").join(&run).join("managed-output");
    let ready = if inputs.members == 0 {
        assert!(
            matches!(std::fs::read(&record), Err(e) if e.kind()==std::io::ErrorKind::NotFound),
            "zero active members must not publish capture metadata"
        );
        Ready::NoManagedMembers { run }
    } else {
        let text = std::fs::read_to_string(&record).unwrap();
        let metadata = Metadata::parse(&text).unwrap_or_else(|| panic!("trusted scoped owner metadata"));
        assert_eq!(metadata.run, run);
        assert_eq!(metadata.server, fixture.owner.fingerprint());
        assert_eq!(metadata.owner, fixture.owner.fingerprint());
        let expected: Vec<_> = members.iter().flat_map(|m| [(m.id, 1), (m.id, 2)]).collect();
        assert_eq!(
            metadata.endpoints.keys().copied().collect::<Vec<_>>(),
            expected,
            "files-only declaration hole must remain member0"
        );
        // Published metadata contains public fingerprints and endpoint addresses only.
        std::fs::write(inputs.records.join("managed-output.ready"), text).unwrap();
        let ready = Ready::OwnerChannels {
            run,
            endpoints: metadata.endpoints.len(),
            unique_addresses: metadata
                .endpoints
                .values()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
        };
        fixture.record = Some(record);
        fixture.metadata = Some(metadata);
        ready
    };
    fixture.initial_fds = Some(std::fs::read_dir("/proc/self/fd").unwrap().count());
    (ready, members)
}

#[derive(Debug, Clone)]
enum ObservationFailure {
    Io {
        kind: std::io::ErrorKind,
        code: Option<i32>,
    },
    MalformedIdentityRecord,
    InspectFailed {
        status: Option<i32>,
    },
    MalformedVmPid,
}

impl ObservationFailure {
    fn io(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            code: error.raw_os_error(),
        }
    }
    fn record(&self) -> Value {
        match self {
            Self::Io { kind, code } => {
                json!({"state":"Unavailable","kind":format!("{kind:?}"),"raw_os_code":code})
            }
            Self::MalformedIdentityRecord => json!({"state":"MalformedIdentityRecord"}),
            Self::InspectFailed { status } => json!({"state":"InspectFailed","status":status}),
            Self::MalformedVmPid => json!({"state":"MalformedVmPid"}),
        }
    }
}

#[derive(serde::Serialize)]
enum LinkCoverage {
    CompleteAtEnumeration,
    Partial,
    Unavailable,
}

fn failure(error: std::io::Error) -> Value {
    ObservationFailure::io(error).record()
}

fn read_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn retain(inputs: &Inputs, stem: &str, bytes: std::io::Result<Vec<u8>>) -> Value {
    match bytes {
        Ok(bytes) => {
            std::fs::write(inputs.records.join(stem), &bytes).unwrap();
            json!({"state":"Observed","file":stem,"bytes":bytes.len()})
        }
        Err(error) => failure(error),
    }
}

fn named(path: &Path) -> Result<shards_ipc::Named, ObservationFailure> {
    let bytes = read_bytes(path).map_err(ObservationFailure::io)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| ObservationFailure::MalformedIdentityRecord)?;
    shards_ipc::Named::parse(text).ok_or(ObservationFailure::MalformedIdentityRecord)
}

fn vm(fixture: &Fixture, inputs: &Inputs, until: Instant) -> Result<shards_ipc::Named, ObservationFailure> {
    let inspected = fixture.command(
        &["inspect", "vm", "-f", "{{.State.Pid}}", "owner-output"],
        left(until),
    );
    save_command(inputs, "inspect.json", &inspected);
    if inspected.status != Some(0) {
        return Err(ObservationFailure::InspectFailed {
            status: inspected.status,
        });
    }
    let pid = inspected
        .stdout
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or(ObservationFailure::MalformedVmPid)?;
    let start = shards_ipc::started(pid).map_err(ObservationFailure::io)?;
    Ok(shards_ipc::Named {
        pid,
        start: Some(start),
    })
}

fn snapshot(inputs: &Inputs, role: &str, identity: &Result<shards_ipc::Named, ObservationFailure>) -> Value {
    let identity = match identity {
        Ok(identity) => identity,
        Err(error) => return error.record(),
    };
    // An opened proc directory keeps operations tied to its original task even if
    // that PID is reused. No ordinary pathname re-lookup between observations.
    use std::os::fd::AsRawFd as _;
    let directory = match std::fs::File::open(format!("/proc/{}", identity.pid)) {
        Ok(directory) => directory,
        Err(error) => return failure(error),
    };
    let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let before = read_bytes(&root.join("stat"));
    let before_identity = before.as_ref().ok().and_then(|b| parse::process(b));
    let expected = identity.start.map(|start| (identity.pid, start));
    if expected.is_none() || before_identity != expected {
        return json!({"state":"IdentityUnverifiedBeforeRead","pid":identity.pid,"expected_start":identity.start,"observed_identity":before_identity,"read_error":before.err().map(failure)});
    }
    let mut fields = serde_json::Map::new();
    fields.insert(
        "stat_before".into(),
        retain(inputs, &format!("{role}.stat-before"), before),
    );
    for file in ["status", "smaps_rollup", "cgroup"] {
        fields.insert(
            file.into(),
            retain(inputs, &format!("{role}.{file}"), read_bytes(&root.join(file))),
        );
    }
    let mut inodes = BTreeSet::new();
    let mut coverage = LinkCoverage::CompleteAtEnumeration;
    let fds = match std::fs::read_dir(root.join("fd")) {
        Err(error) => {
            coverage = LinkCoverage::Unavailable;
            failure(error)
        }
        Ok(entries) => {
            let mut rows = Vec::new();
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        coverage = LinkCoverage::Partial;
                        rows.push(failure(error));
                        continue;
                    }
                };
                let fd = entry.file_name().to_string_lossy().into_owned();
                match std::fs::read_link(entry.path()) {
                    Err(error) => {
                        coverage = LinkCoverage::Partial;
                        rows.push(json!({"fd":fd,"observation":failure(error)}));
                    }
                    Ok(target) => {
                        use std::os::unix::ffi::OsStrExt as _;
                        let bytes = target.as_os_str().as_bytes();
                        let inode = parse::socket_inode(bytes);
                        if let Some(inode) = inode {
                            inodes.insert(inode);
                        }
                        // Never retain ordinary pathnames, credential paths or key contents.
                        let kind = if inode.is_some() {
                            "Socket"
                        } else if bytes.starts_with(b"pipe:[") {
                            "Pipe"
                        } else if bytes.starts_with(b"anon_inode:") {
                            "Anonymous"
                        } else {
                            "OtherTargetOmitted"
                        };
                        rows.push(json!({"fd":fd,"kind":kind,"socket_inode":inode}));
                    }
                }
            }
            json!({"state":"ObservedEntriesNonAtomic","entry_count":rows.len(),"entries":rows})
        }
    };
    fields.insert("fds".into(), json!({"observations":fds,"link_coverage":coverage,"known_socket_inodes":inodes,"known_socket_inode_count":inodes.len(),"complete_socket_count":"Unverified: non-atomic enumeration; failures are explicit"}));
    for table in ["udp", "udp6"] {
        let observation = match read_bytes(&root.join("net").join(table)) {
            Err(error) => failure(error),
            Ok(bytes) => match parse::owned_udp(&bytes, &inodes) {
                None => json!({"state":"MalformedSocketTable"}),
                Some(rows) => {
                    let mut owned = Vec::new();
                    for row in &rows {
                        owned.extend_from_slice(row);
                        owned.push(b'\n');
                    }
                    json!({"retained":retain(inputs, &format!("{role}.{table}-owned"), Ok(owned)),"matched_row_count":rows.len(),"fd_link_coverage":coverage,"scope":"only rows matching known sampled owned FD socket inodes; non-atomic, not a complete live socket count"})
                }
            },
        };
        fields.insert(table.into(), observation);
    }
    let after = read_bytes(&root.join("stat"));
    let after_identity = after.as_ref().ok().and_then(|b| parse::process(b));
    fields.insert(
        "stat_after".into(),
        retain(inputs, &format!("{role}.stat-after"), after),
    );
    let state = if expected.is_some() && before_identity == expected && after_identity == expected {
        "MatchedBeforeAndAfter"
    } else {
        "IdentityUnverified"
    };
    json!({"pid":identity.pid,"expected_start":identity.start,"before_identity":before_identity,"after_identity":after_identity,"identity_state":state,"observations":fields,"atomic":false,"cgroup_kernel_attribution":"Unverified: proc membership only; no isolated per-member socket charge proof"})
}

#[derive(Debug, Clone)]
enum ExitCause {
    Observation(ObservationFailure),
    MissingExpectedStart,
    MalformedPinnedStat,
    DifferentIdentity {
        expected: (u32, u64),
        observed: (u32, u64),
    },
    FdMismatch {
        expected: usize,
        observed: usize,
    },
    CounterOverflow,
    NotPrepared,
}

#[derive(Debug, Clone)]
struct ExitFailure {
    stage: &'static str,
    cause: ExitCause,
}

impl ExitFailure {
    fn io(stage: &'static str, error: std::io::Error) -> Self {
        Self {
            stage,
            cause: ExitCause::Observation(ObservationFailure::io(error)),
        }
    }

    fn record(&self) -> Value {
        let cause = match &self.cause {
            ExitCause::Observation(error) => error.record(),
            ExitCause::MissingExpectedStart => json!({"state":"MissingExpectedStart"}),
            ExitCause::MalformedPinnedStat => json!({"state":"MalformedPinnedStat"}),
            ExitCause::DifferentIdentity { expected, observed } => {
                json!({"state":"DifferentIdentity","expected":expected,"observed":observed})
            }
            ExitCause::FdMismatch { expected, observed } => {
                json!({"state":"FdMismatch","expected":expected,"observed":observed})
            }
            ExitCause::CounterOverflow => json!({"state":"CounterOverflow"}),
            ExitCause::NotPrepared => json!({"state":"NotPrepared"}),
        };
        json!({"stage":self.stage,"cause":cause})
    }
}

fn fd_count(stage: &'static str) -> Result<usize, ExitFailure> {
    let entries = std::fs::read_dir("/proc/self/fd").map_err(|e| ExitFailure::io(stage, e))?;
    let mut count = 0usize;
    for entry in entries {
        entry.map_err(|e| ExitFailure::io(stage, e))?;
        count = count.checked_add(1).ok_or(ExitFailure {
            stage,
            cause: ExitCause::CounterOverflow,
        })?;
    }
    Ok(count)
}

fn fd_record(count: &Result<usize, ExitFailure>) -> Value {
    match count {
        Ok(count) => json!({"state":"Observed","count":count}),
        Err(error) => json!({"state":"Unavailable","failure":error.record()}),
    }
}

struct PinnedProcess {
    expected: shards_ipc::Named,
    directory: std::fs::File,
}

impl PinnedProcess {
    fn open(expected: shards_ipc::Named) -> Result<Self, ExitFailure> {
        if expected.start.is_none() {
            return Err(ExitFailure {
                stage: "open_identity",
                cause: ExitCause::MissingExpectedStart,
            });
        }
        let directory = std::fs::File::open(format!("/proc/{}", expected.pid))
            .map_err(|e| ExitFailure::io("open_identity", e))?;
        Ok(Self { expected, directory })
    }

    fn check(&self, stage: &'static str) -> Result<(), ExitFailure> {
        use std::os::fd::AsRawFd as _;
        let bytes = std::fs::read(format!("/proc/self/fd/{}/stat", self.directory.as_raw_fd()))
            .map_err(|e| ExitFailure::io(stage, e))?;
        let observed = parse::process(&bytes).ok_or(ExitFailure {
            stage,
            cause: ExitCause::MalformedPinnedStat,
        })?;
        let started = self.expected.start.ok_or(ExitFailure {
            stage,
            cause: ExitCause::MissingExpectedStart,
        })?;
        let expected = (self.expected.pid, started);
        if observed != expected {
            return Err(ExitFailure {
                stage,
                cause: ExitCause::DifferentIdentity { expected, observed },
            });
        }
        Ok(())
    }
}

enum ReadinessIdentity {
    Pending,
    Matched,
    Unverified(ExitFailure),
}

enum HostExit {
    Unavailable {
        expected: Option<shards_ipc::Named>,
        failure: ExitFailure,
    },
    Watching {
        identity: PinnedProcess,
        _watch: shards_vmm::platform::ExitWatch,
        readiness: ReadinessIdentity,
    },
    ExitReady {
        pid: u32,
        started: Option<u64>,
    },
    Deadline {
        pid: u32,
        started: Option<u64>,
    },
}

impl HostExit {
    fn acquire(
        poller: &shards_vmm::platform::Poller,
        expected: &Result<shards_ipc::Named, ObservationFailure>,
        token: u64,
    ) -> Self {
        let acquire = || {
            let expected = expected.as_ref().map_err(|e| ExitFailure {
                stage: "expected_identity",
                cause: ExitCause::Observation(e.clone()),
            })?;
            let identity = PinnedProcess::open(*expected)?;
            identity.check("before_watch")?;
            let watch = poller
                .add_exit(expected.pid, token)
                .map_err(|e| ExitFailure::io("add_exit", e))?;
            identity.check("after_watch")?;
            Ok((identity, watch))
        };
        match acquire() {
            Ok((identity, watch)) => Self::Watching {
                identity,
                _watch: watch,
                readiness: ReadinessIdentity::Pending,
            },
            Err(failure) => Self::Unavailable {
                expected: expected.as_ref().ok().copied(),
                failure,
            },
        }
    }

    fn retained_fds(&self) -> usize {
        match self {
            // Each actual Watching value owns one proc directory and one ExitWatch
            // (the existing Linux implementation owns one unique pidfd).
            Self::Watching { .. } => 2,
            _ => 0,
        }
    }

    fn validate_readiness(&mut self) {
        if let Self::Watching {
            identity, readiness, ..
        } = self
        {
            *readiness = match identity.check("final_readiness") {
                Ok(()) => ReadinessIdentity::Matched,
                Err(error) => ReadinessIdentity::Unverified(error),
            };
        }
    }

    fn awaiting(&self) -> bool {
        matches!(
            self,
            Self::Watching {
                readiness: ReadinessIdentity::Matched,
                ..
            }
        )
    }

    fn retire(self, event: bool, expired: bool) -> Self {
        match self {
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Unverified(failure),
                ..
            } => Self::Unavailable {
                expected: Some(identity.expected),
                failure,
            },
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Pending,
                ..
            } => Self::Unavailable {
                expected: Some(identity.expected),
                failure: ExitFailure {
                    stage: "final_readiness",
                    cause: ExitCause::NotPrepared,
                },
            },
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Matched,
                ..
            } if event => Self::ExitReady {
                pid: identity.expected.pid,
                started: identity.expected.start,
            },
            Self::Watching {
                identity,
                readiness: ReadinessIdentity::Matched,
                ..
            } if expired => Self::Deadline {
                pid: identity.expected.pid,
                started: identity.expected.start,
            },
            state => state,
        }
    }

    fn failed(self, error: &ExitFailure) -> Self {
        match self {
            Self::Watching { identity, .. } => Self::Unavailable {
                expected: Some(identity.expected),
                failure: error.clone(),
            },
            state => state,
        }
    }

    fn record(&self) -> Value {
        match self {
            Self::Unavailable { expected, failure } => {
                json!({"state":"Unavailable","expected_identity":expected.map(|n|json!({"pid":n.pid,"started":n.start})),"failure":failure.record()})
            }
            Self::Watching {
                identity, readiness, ..
            } => {
                json!({"state":"Watching","pid":identity.expected.pid,"expected_started":identity.expected.start,"readiness_identity":match readiness { ReadinessIdentity::Pending=>json!({"state":"Pending"}),ReadinessIdentity::Matched=>json!({"state":"Matched"}),ReadinessIdentity::Unverified(e)=>e.record() },"retained_fds":self.retained_fds()})
            }
            Self::ExitReady { pid, started } => {
                json!({"state":"ExitReadyObservedAfterOwnerStopWithinCleanupDeadline","pid":pid,"expected_started":started,"reaped":"Unverified"})
            }
            Self::Deadline { pid, started } => {
                json!({"state":"Deadline","pid":pid,"expected_started":started})
            }
        }
    }
}

struct ExitProof {
    poller: Result<shards_vmm::platform::Poller, ExitFailure>,
    daemon: HostExit,
    vm: HostExit,
    before: Result<usize, ExitFailure>,
    original_observer_baseline: Option<usize>,
    retained: Result<usize, ExitFailure>,
    delta: Result<(), ExitFailure>,
}

impl ExitProof {
    fn acquire(
        fixture: &mut Fixture,
        daemon: &Result<shards_ipc::Named, ObservationFailure>,
        vm: &Result<shards_ipc::Named, ObservationFailure>,
    ) -> Self {
        let original_observer_baseline = fixture.initial_fds;
        let before = fd_count("before_exit_handles");
        let poller = shards_vmm::platform::Poller::new().map_err(|e| ExitFailure::io("poller", e));
        let (daemon, vm) = match &poller {
            // These tokens name the two fixed owned roles, not a resource limit.
            Ok(poller) => (
                HostExit::acquire(poller, daemon, 1),
                HostExit::acquire(poller, vm, 2),
            ),
            Err(error) => (
                HostExit::Unavailable {
                    expected: daemon.as_ref().ok().copied(),
                    failure: error.clone(),
                },
                HostExit::Unavailable {
                    expected: vm.as_ref().ok().copied(),
                    failure: error.clone(),
                },
            ),
        };
        // Existing Linux Poller owns one epoll FD; add only resources actually held.
        let held = usize::from(poller.is_ok()) + daemon.retained_fds() + vm.retained_fds();
        let retained = fd_count("with_exit_handles");
        let delta = (|| {
            let before = before.as_ref().map_err(Clone::clone)?;
            let original = original_observer_baseline.ok_or(ExitFailure {
                stage: "original_observer_baseline",
                cause: ExitCause::NotPrepared,
            })?;
            if original != *before {
                return Err(ExitFailure {
                    stage: "before_exit_handles",
                    cause: ExitCause::FdMismatch {
                        expected: original,
                        observed: *before,
                    },
                });
            }
            let actual = retained.as_ref().map_err(Clone::clone)?;
            let expected = before.checked_add(held).ok_or(ExitFailure {
                stage: "exit_handle_delta",
                cause: ExitCause::CounterOverflow,
            })?;
            if expected != *actual {
                return Err(ExitFailure {
                    stage: "exit_handle_delta",
                    cause: ExitCause::FdMismatch {
                        expected,
                        observed: *actual,
                    },
                });
            }
            Ok(())
        })();
        if let Ok(retained) = retained {
            // Exact observer-return equality remains unchanged while these objects live.
            // Total return to the independent pre-watch baseline is checked after drop.
            fixture.initial_fds = Some(retained);
        }
        Self {
            poller,
            daemon,
            vm,
            before,
            original_observer_baseline,
            retained,
            delta,
        }
    }

    fn ready_record(&mut self) -> Value {
        self.daemon.validate_readiness();
        self.vm.validate_readiness();
        json!({"daemon":self.daemon.record(),"vm":self.vm.record(),"original_observer_baseline":self.original_observer_baseline,"before_exit_handles":fd_record(&self.before),"with_exit_handles":fd_record(&self.retained),"owned_fd_delta":match &self.delta { Ok(())=>json!({"state":"Matched"}),Err(e)=>e.record() },"observer_return":"unchanged exact equality uses measured with-handle baseline; original baseline match and total after-release equality also required"})
    }

    fn finish(self, until: Instant) -> (Value, bool, Result<usize, ExitFailure>) {
        let Self {
            poller,
            daemon,
            vm,
            before,
            original_observer_baseline,
            retained,
            delta,
        } = self;
        // Invalid readiness evidence is never upgraded by a subsequent exit event.
        let mut states = (daemon.retire(false, false), vm.retire(false, false));
        if let Ok(poller) = &poller {
            let mut ready = Vec::new();
            while states.0.awaiting() || states.1.awaiting() {
                let remaining = left(until);
                if remaining.is_zero() {
                    states = (states.0.retire(false, true), states.1.retire(false, true));
                    break;
                }
                if let Err(error) = poller.wait(&mut ready, Some(remaining)) {
                    let failure = ExitFailure::io("wait_exit", error);
                    states = (states.0.failed(&failure), states.1.failed(&failure));
                    break;
                }
                // A scheduling delay beyond the caller's deadline is conservatively
                // Deadline, even if readiness might have arisen earlier in the kernel.
                let expired = Instant::now() > until;
                states = (
                    states.0.retire(!expired && ready.contains(&1), expired),
                    states.1.retire(!expired && ready.contains(&2), expired),
                );
                // Each consumed watch closes now, removing level-triggered readiness.
                // Empty/EINTR waits only re-enter the same event wait with time remaining.
            }
        }
        let passed = matches!(states.0, HostExit::ExitReady { .. })
            && matches!(states.1, HostExit::ExitReady { .. })
            && delta.is_ok();
        let record = json!({"daemon":states.0.record(),"vm":states.1.record(),"original_observer_baseline":original_observer_baseline,"before_exit_handles":fd_record(&before),"observer_return_baseline_with_handles":fd_record(&retained),"owned_fd_delta":match delta { Ok(())=>json!({"state":"Matched"}),Err(e)=>e.record() },"strict_exit_proof":passed,"semantics":"exit readiness only; no exit status/time/causality, reaping, full-population or kernel-return proof"});
        drop(states);
        drop(poller);
        (record, passed, before)
    }
}

fn run(fixture: &mut Fixture, inputs: &Inputs, until: Instant, exits: &mut Option<ExitProof>) -> Ready {
    let (ready, members) = start(fixture, inputs, until);
    let daemon = named(&fixture.home.join(shards_ipc::PID));
    let vm = vm(fixture, inputs, until);
    *exits = Some(ExitProof::acquire(fixture, &daemon, &vm));
    let observer = shards_ipc::started(std::process::id())
        .map(|start| shards_ipc::Named {
            pid: std::process::id(),
            start: Some(start),
        })
        .map_err(ObservationFailure::io);
    let mut channels = Vec::new();
    if let Ready::OwnerChannels { .. } = &ready {
        // Establish every authenticated observer before consuming any channel.
        for member in &members {
            for channel in [Channel::Stdout, Channel::Stderr] {
                let index = fixture.observe(member.id, channel, None, false, until);
                channels.push((member, index, channel));
            }
        }
        for (member, index, channel) in &channels {
            let (start, end) =
                fixture.observers[*index].exact(&member.label, &payload(&member.token, *channel), until);
            let held = member.id == 1 && *channel == Channel::Stdout;
            if !held {
                fixture.observers[*index]
                    .sender
                    .send(Command::Handler(Ask::Ack(end)))
                    .unwrap();
            }
            fixture
                .observed
                .insert((member.id, *channel as u8), (end, if held { start } else { end }));
        }
    }
    fixture.main_progress(left(until));
    if let Some(proof) = exits {
        inputs.write("exit-watches.ready.json", &proof.ready_record());
    }
    inputs.write("ready.json", &ready.record());
    let sampled = json!({"checkpoint":ready.record(),"daemon":snapshot(inputs,"daemon",&daemon),"vm":snapshot(inputs,"vm",&vm),"observer_parent":snapshot(inputs,"observer",&observer),"measurement_cost":"one-shot proc reads/serialization and observer drivers are fixture costs; samples are not atomic"});
    inputs.write("snapshots.json", &sampled);
    for (member, index, channel) in &channels {
        let observer = &mut fixture.observers[*index];
        if member.id == 1 && *channel == Channel::Stdout {
            observer.sender.send(Command::Handler(Ask::Cancel)).unwrap();
            observer.join(until).unwrap();
        } else {
            // A repeated consumed cursor is legal; FIN proves processing before close.
            let wire = fixture.observed[&(member.id, *channel as u8)].0;
            observer.ack_and_finish(wire, until);
        }
    }
    fixture.main_progress(left(until));
    inputs.write("control.json", &json!({"state":"MainProgressAfterObserverCancel","stalled_member":if inputs.members>0 { Some(1) } else { None },"members":inputs.members,"exact_payload_channels":channels.len(),"terminal_consumption":"verified after owner stop; before this point non-held ACKs were sent"}));
    ready
}

#[test]
#[ignore = "coordinator-only explicit member count/deadline/records; real VM resource observations"]
fn owner_output_resources_at_explicit_member_count() {
    let inputs = Inputs::supplied();
    assert!(
        !common::cannot_run_vms(),
        "resource fixture requires a real supported VM host"
    );
    let until = Instant::now().checked_add(inputs.patience).unwrap();
    let artifact =
        |path: &Path| json!({"path":path,"sha256":common::sha256_digest(&std::fs::read(path).unwrap())});
    inputs.write("inputs.json", &json!({"members":inputs.members,"phase_deadline_ms":inputs.patience.as_millis(),"source":option_env!("SHARDS_OUTPUT_COST_SOURCE_REVISION"),"fixture":artifact(&std::env::current_exe().unwrap()),"cli":artifact(common::shards()),"testguest":artifact(common::test_guest()),"kernel":artifact(common::kernel()),"init":artifact(common::guest_init()),"topology":"EXPERIMENTAL2N; unadopted","root_cost_comparison":"not executed by implementer"}));
    let mut fixture = Fixture::new();
    // None means acquisition was not reached. Some owns all observation resources
    // outside the protocol unwind boundary, including partial acquisition failures.
    let mut exits = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut fixture, &inputs, until, &mut exits)
    }));
    // Cleanup is a separate explicitly recorded finite phase, using caller patience.
    let cleanup_until = Instant::now().checked_add(inputs.patience).unwrap();
    let stopped = fixture.shutdown(cleanup_until);
    let (exit_record, exit_passed, before) = match exits {
        Some(proof) => proof.finish(cleanup_until),
        None => {
            let failure = ExitFailure {
                stage: "exit_acquisition",
                cause: ExitCause::NotPrepared,
            };
            (failure.record(), false, Err(failure))
        }
    };
    // finish has dropped every watch, pinned identity directory and poller before
    // this independent total-FD observation and assertion, including on failures.
    let released = fd_count("after_exit_handle_release");
    let fd_returned = matches!((&before,&released),(Ok(before),Ok(after)) if before==after);
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            fd_returned,
            "exit observation total FD return: before={before:?}, after={released:?}"
        );
        assert!(
            exit_passed,
            "both validated owned exit watches must report readiness before cleanup deadline: {exit_record}"
        );
    }));
    inputs.write("teardown.json", &json!({"owner_stop_and_observer_join":stopped,"protocol_completed":result.is_ok(),"cleanup_phase_deadline_ms":inputs.patience.as_millis(),"host_process_exit":exit_record,"total_fds_before_exit_handles":fd_record(&before),"total_fds_after_release":fd_record(&released),"total_fd_return":fd_returned,"host_process_exit_and_kernel_resource_return":"See held exit readiness proof; kernel/full-population return remain Unverified"}));
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    if let Err(error) = checked {
        std::panic::resume_unwind(error);
    }
    stopped.unwrap();
    let ready = result.unwrap();
    if let Ready::OwnerChannels { .. } = ready {
        let terminal: Vec<_> = fixture.observed.iter().map(|(&(member,channel),expected)| {
            let actual=fixture.terminal(member,Channel::try_from(channel).unwrap());
            assert_eq!(&actual,expected);
            json!({"member":member,"channel":channel,"accepted":actual.0,"consumed":actual.1,"tail":"unknown"})
        }).collect();
        inputs.write(
            "terminal.json",
            &json!({"state":"OwnerChannelsAborted","channels":terminal}),
        );
    } else {
        inputs.write(
            "terminal.json",
            &json!({"state":"NoManagedMembers","managed_channels":0}),
        );
    }
}

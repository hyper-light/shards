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
        members
            .checked_add(1)
            .expect("files-only declaration and members must fit u32");
        usize::try_from(members)
            .unwrap()
            .checked_mul(2)
            .expect("channel count must fit usize");
        let patience = Duration::from_millis(required("SHARDS_OUTPUT_COST_DEADLINE_MS").parse().unwrap());
        assert!(
            !patience.is_zero(),
            "explicit finite positive phase deadline required"
        );
        Instant::now()
            .checked_add(patience)
            .expect("representable phase deadline");
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
        let metadata = Metadata::parse(&text).expect("trusted scoped owner metadata");
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

#[derive(Debug)]
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

fn run(fixture: &mut Fixture, inputs: &Inputs, until: Instant) -> Ready {
    let (ready, members) = start(fixture, inputs, until);
    let daemon = named(&fixture.home.join(shards_ipc::PID));
    let vm = vm(fixture, inputs, until);
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut fixture, &inputs, until)));
    // Cleanup is a separate explicitly recorded finite phase, using caller patience.
    let cleanup_until = Instant::now().checked_add(inputs.patience).unwrap();
    let stopped = fixture.shutdown(cleanup_until);
    inputs.write("teardown.json", &json!({"owner_stop_and_observer_join":stopped,"protocol_completed":result.is_ok(),"cleanup_phase_deadline_ms":inputs.patience.as_millis(),"host_process_exit_and_kernel_resource_return":"Unverified; stop result and observer FD equality are narrower facts"}));
    if let Err(error) = result {
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

# Normal-exit fixture: explicitly publish its existing owner gate

Separate caller-only fix based exactly on fce89fb1063b5c2344f920ff9e967c023ce3abfc.
It contains neither crash87503 nor reconnect0f0b source. ROOT explicitly authorized
the optional publication argument on2026-11-10 after actual r632 reproduced the
missing TCP port. Policy readback: IMPLEMENTER01a1c5d28cea74a5b0241200190d6c54;
coordinator01a148884a4770fdaf0111f3f792a248; canonical policy files f5cec365,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Full canonical policy/evidence/CLAUDE/managed-plan/handoff read; R01–R16 apply.

ROOT baseline source f31f58f4df913323dbc2d3450ede7bc60405d433, run r632:
both member1 canonical End/ACK/QUIC EOF and earlier main exec succeeded, then
release_neighbor found no TCP gate. Actual test exit101; recorder driver exit0
does NOT mean test pass. r632/ retains SHA256-checked ROOT ports.json, the246-byte
Agentfile and member-ended.json copied read-only from
~/root-managed-normal-f31f-v1/normal-tenant-exit/records. ports.json status0 lists
only the four managed UDP endpoints. ROOT reported owner stop/join true and FD7→7;
protocol and private-run removal were not reached. This lane ran no VM/helper.

Source diagnosis: managed_limits.rs397–415 separately creates
127.0.0.1::{guest_port} and passes run -p for both control ports. Our original
start_with_options supplied no -p. Agentfile declarations grant/select receivers;
they do not request host listeners. daemon::publish::wanted takes bindings from
run.publish unless -P is selected; bind_one/bind_all choose a host port for an
empty host-port field. read_ports lists actual bound port records and then the
separate managed-output endpoints. D122 describes receiver selection, not implicit
host publication. No product networking defect or privilege change is inferred.

The parent now accepts Option<u16> publish_port; None keeps its exact old run
arguments. Some(nonzero) adds only -p127.0.0.1::{guest_port} through the existing
CLI path. The normal tenant caller supplies Some(inputs.port). NETWORK/EXPOSE/
CONNECT are unchanged, host port remains OS-assigned and release_neighbor still
reads the actual published endpoint. The normal fixture file is byte-identical
to fce except that single Some argument: End/EOF, terminal metadata, causal later
sibling bytes, main progress and cleanup assertions are unchanged. No new proxy,
manager, guest helper, grant, profile, trust/crypto or production file is edited.

Light run r15 on Fedora8fadc1ec213332e16cb0d915a3fb913f requested Kiln
linux/amd64,cpus=1,mem=1g,disk=2g. NEW private
~/managed-tenant-publish-light-01a1c5d2-v1/{source,target}; source352KiB,
target81,324KiB. GNU/musl cargo check --tests and clippy --tests -Dwarnings all0;
repository formatter and required policy script passed. No test body/helper was
executed. light.json retains exact commands' logs, toolchain/lock/source hashes
and the compiled publication/run blocks with inert surrounding glue. Normal
fixture/helper bodies are exact; full host integration is ROOT-owed. Headroom is
not measured hard aggregate accounting. No local builds/new full worktree, ROOT
target/cache mutation or paused Windows ARM change. Evidence/source preserved.

ROOT next applies this small patch to its finish-fixed source, builds the actual
normal caller and reruns the original exact normal-exit test with the same explicit
guest port/deadline/new records directory. Expected new witness is an actual
loopback TCP publication and successful owner release AFTER both member1 EOFs,
then exact new sibling stdout/stderr and independent main progress. Full terminal
metadata/cleanup, negative/mutation and seven-target/real-host results remain
unrun here; ROOT owns actual before/after and all audits/profiles/heavy gates.
When later composing87503, retain BOTH independent task Option<u32> and publication
Option<u16> parameters; do not reinterpret the earlier task argument as a port.

| Rule | Scoped evidence / remaining ROOT gate |
| --- | --- |
| R01 | Host test-only assertions; production unchanged; compile/clippy0 scoped, no global panic proof. |
| R02 | ROOT actual r632 plus existing managed_limits/parser/publisher/D122 path; no architecture decision. |
| R03 | Caller nonzero guest port; explicit ROOT-granted loopback; existing OS-assigned host port, no picked port/default. |
| R04 | Explicit Some/None; existing canonical states/strict terminal checks unchanged. |
| R05 | Existing run -p/listener ownership/read-ports path, same fixed deadlines; no manager/cache/polling. |
| R06 | Two real argument variants, scoped compile/clippy; combined review ROOT-owed. |
| R07 | Rationale recorded here; no unneeded code narration. |
| R08 | No comparative/performance claim. |
| R09 | Exact ROOT r632 missing-port red retained; actual fixed green still owed. |
| R10 | Retained actual baseline, repeatable fixture and light logs/hashes; live after/mutation owed. |
| R11 | GNU/musl compile only; actual seven-target/KVM/HVF/WHP not passed for this source. Mac ARM64 only. |
| R12 | Existing shared CLI/publication seam; no OS/security/grant/product change. |
| R13 | Parent parameter + normal caller + evidence only, no crash/reconnect coupling. |
| R14 | No profile/benchmark/allocation/fault/leak/aggregate claim; ROOT owns measurement. |
| R15 | No subagents/actual helper/audit/heavy gate/main merge; signed named-path own branch, ROOT reviews/runs/merges. |
| R16 | Existing worktree reused, private admitted remote light only; local builds held through floor dips; protected work preserved. |

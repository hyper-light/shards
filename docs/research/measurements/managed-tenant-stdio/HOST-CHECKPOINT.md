# Actor stdio closure: complete case B source, light checks only

IMPLEMENTER `01a1c5d28cea74a5b0241200190d6c54`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Re-read all canonical policy/plan/handoff
after context loss at canonical HEAD `1d0588b28f839fd0fec049e4bcb00e261e28c3aa`.
Rules SHA256 `d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`
is unchanged. This is scoped fixture implementation, not a runtime bug fix or
managed-runtime closure. No helper, test body, socket flow or VM was executed here.

## Source and real variants

Branch `p/managed-tenant-stdio-host` starts from the separate normal fixture
`fce89fb1` + publication `96b1019f6` + final-ACK caller `b324ef0a9`. The preserved
native B checkpoint `67f26d91` was signed onto it as `3a3b441d7`. Neither crash875
nor the reconnect candidate is coupled into this patch. Exact final source hashes
are in [host-source.sha256](host-source.sha256).

The new leaf `tenant_stdio.rs` is wired inside our owned `tenant_exit.rs`, leaving
ROOT's parent module/header/global-allocator and cost fixtures alone. The only
additional parent change is the already authorized optional publication input:
`Option<&[u16]>` serves actual None, one-port normal and two-port stdio callers.
Each supplied nonzero guest port becomes an existing `-p 127.0.0.1::{guest}`;
the OS assigns the host port. The None invocation is unchanged. This does not
alter NETWORK/EXPOSE/CONNECT semantics, grants, profiles or product networking.
Crash's approved optional task input remains a distinct future composition item.

The literal native B mode remains the exact previously checked helper:
`managed-capture TOKEN close-stdio PORT WORK_MS CHALLENGE_BYTES REPORT_BYTES`.
Its extracted source SHA256 is
`c7d64d97df91f7363269514026fd21747f4e3bd74e80179bfbf616d8aacb8367`.
It validates its finite work/report envelope before effects, binds the declared
listener, emits and flushes the two prefixes, closes only its own fd1/fd2, accepts
one challenge, echoes its exact tagged u32-length frame, then remains in the
original owned hold. No child is involved. Native failure returns its last phase;
the exact remote IO cause may be Unknown. No stdio diagnostic follows closure,
because accepted sockets can reuse fd1/fd2 numbers.

The host uses the existing checked repeated-ingress Agentfile form from
`managed_limits.rs`, with separate agent/helper and harness/driver EXPOSE and
CONNECT declarations. It retains all four authenticated owner observers and:

1. Requires both actor prefixes, canonical End, ACK and actual transport EOF.
   Raw wire chunks and metadata are retained before assertions. EOF is not a
   population/lifecycle-owner observation.
2. Proves main progress, reads the actual loopback publication and sends a fresh
   caller-supplied challenge whose bytes were not supplied in actor argv. Exact
   echo and control peer EOF must follow the two output EOFs. Partial IO and
   Interrupted retain one original absolute deadline; extra reply bytes are
   retained and fail. No repeated state probes or fresh retry deadline.
3. Proves main progress again, then releases the separate driver gate. Requires
   its exact new fd1/fd2 bytes with FinalPayload leaving the sole final ACK+FIN
   to the existing caller. Proves main progress again and records a visible
   non-zombie actor-UID row after all these events.
4. Always attempts existing stop/inspect/remove and owner shutdown/observer join
   under the cleanup deadline fixed before effects. Retains teardown results
   before rethrowing failures, then applies the unchanged strict terminal metadata
   function. Actor output must be ended/complete; live driver owner-stop remains
   aborted/unknown. A visible row, observer FD return and output EOF do not prove
   full population reaping or kernel resource return.

The supplied byte envelope covers the actor capture prefixes and request+reply
frame bytes only. It is not a hard aggregate parent/kernel allocation budget.
Fixture copies, trace files, observers and artifact hashes have costs; no measured
allocation, leak or performance claim follows from this source check.

## Exact light evidence

Fedora machine `8fadc1ec213332e16cb0d915a3fb913f`, r26; NEW private
`~/managed-tenant-stdio-host-light-01a1c5d2-v3/{source,target}` under Kiln admission
`linux/amd64,cpus=1,mem=1g,disk=2g`, Cargo jobs1/offline. Admission is not hard
aggregate accounting. Source392KiB; target83,088KiB. Rust/cargo1.99.1,
Fedora44/kernel6.19.10. [v3 raw](host-raw/v3.json) retains toolchain, lockfile,
formatted-original/compiled-source hashes, outputs and the exact compiled publication
blocks; [command](host-kiln-v3.command) retains the source transfer hash.

GNU and musl `cargo check --tests` and `cargo clippy --tests -- -D warnings` all
exit0. Formatter and policy checker exit0. They compile the exact final native
helper, ABI and host leaf bodies with explicitly inert surrounding integration
stubs. Exact parent publication/run blocks are separately typechecked in that
same harness. This is not a full actual host/guest build, dispatcher validation,
grant/seccomp validation or execution result. `prepare-host-light.py` reproduces
the source extraction; the original prepare-light.py supplies its inert wrapper.
Local checked source matches the recorded formatted original and compiled-leaf
hashes. The strict terminal function is byte-identical to the b324 parent.

[v1 raw](host-raw/v1.json) retains clippy101 (unused imported IO traits and hex
chunk style), despite compile0. Corrected [v2](host-raw/v2.json) has all four
compile/clippy0; v3 additionally retains an unexpected control reply byte before
failing and is the final checked source. Earlier results are not substituted for
the final revision. No local Cargo/build/full worktree or protected cache change.

## ROOT actual gates and repeatable inputs

ROOT's actual normal r668/eaf0 confirms publication96 and final-ACK b324: both
member1 End/ACK/EOF, causal sibling bytes, main progress, remove, owner join and
FD7→7 succeeded. It then fails STRICT final metadata `(aborted,unknown)` versus
required `(ended,complete)`. Main4bface contains the exact red. The refactor lane
owns its authorized product correction. This fixture preserves the strict
function without claiming that correction or the new B runtime result.

ROOT rebuilds source-matching host and every carried helper/ABI, retains actual
artifact hashes and runs the ignored exact test after its normal gate:
ROOT retains the parent `tenant_exit` module-header wiring used by its normal
probe; this branch deliberately does not edit that ROOT-owned header.

```sh
SHARDS_TENANT_STDIO_PORT="$caller_sibling_guest_port" \
SHARDS_TENANT_STDIO_ACTOR_PORT="$caller_actor_guest_port" \
SHARDS_TENANT_STDIO_DEADLINE_MS="$caller_host_work_ms" \
SHARDS_TENANT_STDIO_WORK_MS="$caller_actor_work_ms" \
SHARDS_TENANT_STDIO_CLEANUP_MS="$caller_cleanup_ms" \
SHARDS_TENANT_STDIO_REPORT_BYTES="$caller_report_bytes" \
SHARDS_TENANT_STDIO_CHALLENGE_HEX="$caller_fresh_challenge_hex" \
SHARDS_TENANT_STDIO_RECORDS="$new_absolute_records" \
SHARDS_TENANT_STDIO_SOURCE_REVISION="$exact_combined_source" \
cargo test -p shards --test managed_output --profile test-release \
  tenant_exit::stdio::actor_stdio_eof_preserves_actor_control_and_neighbor_lifetime \
  -- --ignored --exact --nocapture --test-threads=1
```

All inputs are explicit, distinct nonzero guest ports, positive finite deadlines,
nonempty even hex/u32-sized challenge and checked sufficient report bound.
ROOT supplies its existing source-matching kernel/init/testguest configuration;
no sampled/default production number is selected here. The records directory
must be new and absolute. Source revision is compile-time `option_env!` and
must be set at the actual source build, not merely when reusing a binary.

ROOT owes actual source-matching compatibility, canonical-End/ACK/EOF sequence,
fresh challenge and sibling/main causal progress, wrong-byte/early-End/early-actor-
death guards and restored passes, actual owned cleanup/population/FD/kernel
outcomes, seven-target and supported real-host gates. Missing listener, seccomp
or grant refusal stays a fixture failure; no profile widening. Case A inherited
writers remains next; reconnect body awaits the separately proposed initial
cursor seam outside our authorized parent edit. No lane completion claim.

## R01–R16 accounting

| Rule | Scoped evidence and remaining gap |
| --- | --- |
| R01 | No product edit; native typed IO/checked framing, prior helper panic-denial clippy0. Tests assert. No universal implicit-panic proof. |
| R02 | Original PROPOSAL primary pipe/close/Stdio/Child sources and current checked Agentfile/publication callers justify these fixture variants. Actual behavior still ROOT-owed. |
| R03 | Caller supplies budgets/ports/challenge; tag1 and u32 width define the literal fixture frame. No new production/default constant. |
| R04 | Explicit native phases, three existing read goals and recorded causal milestones; typed IO error retained. Remote exact cause/population uncertainty explicit. |
| R05 | Existing observer/owner/publication seam; one versus two real published ports. Readiness/completion and fixed protocol deadlines, no cache/parallel manager. |
| R06 | Focused new leaf and tiny parent input change; four final scoped compile/clippy0. Coordinator combined audit owed. |
| R07 | Comments explain FD reuse/provenance/deadline obligations; no product narration added. |
| R08 | No comparative baseline/ratio/4–5× claim; ROOT benchmarks owed. |
| R09 | New fixture, no guessed product fix. ROOT r632/r662/r668 exact previous failures retain their separate correction records. New B live result unrun. |
| R10 | Exact source/raw failures and final light logs/recipe retained. Actual before/after/mutation/cleanup results owed. |
| R11 | Linux GNU/musl compile only. Seven targets/KVM/HVF/WHP unverified for this source; macOS ARM64 only. Paused Windows ARM untouched. |
| R12 | Shared existing fixture and Linux native helper seams, no new platform adapter, installed binary or authority change. |
| R13 | One finite control exchange and existing healthy gate, focused capture trace option; no new manager or production mechanism. |
| R14 | No profiling/measurement here; parent/kernel/copies/resource distributions and uncertainty ROOT-owed. |
| R15 | No children/automatic dispatch, actual helper/VM/socket run, audit or main merge. Signed named-path own checkpoint; ROOT owns actual validation/review/merge. |
| R16 | Existing worktree and small source snapshots; no local target/cache active or deletion. Private remote light paths only, protected ROOT caches and frozen artifacts preserved. |

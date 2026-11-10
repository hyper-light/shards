# Actual empty-pending FIN result

The ONE approved native stdout/errno5 case passed1/0failed/0ignored at frozen
source d8a2bf8aa4469c8962a825cb8e2154b7c175f091, directly from current main
0342b85d9c808e3ced041a6c3ef0e9bf70438272. Seven focused pure checks and strict
caller Clippy pass on the SAME source; no code changed after freeze.
No VM, mutation, case retry, replacement connection or traffic/resource tuning.

Only the approved three test files changed. Production and positive-write
accounting sources remain byte-identical to main. Role/policy/ownership/primary
alternatives/R01–R16 and remaining ROOT gates are in README.md/source-scope.json.

## Actual state, byte and cleanup witnesses

Fresh native run fd67bd32-3210-4af8-9e56-d26d98fd1b4f used the existing real
Endpoint, Client, Observer, pending Vec, flush, finish and credit notification.
Actual checked canonical Open/Data/Aborted records were received and ACKed,
then actual transport End. Canonical state was Aborted{offset26,errno5},
separately from remote Finished{accepted90,consumed90}.

The same authenticated control stream received one actual empty Vec with
finish:true through Ask::RawControl. Saved postterminal-control.bin is empty.
The actual receipt sequence was exactly:

```json
[{"error":null,"kind":"Finish","pending":0}]
```

The peer supplied actual Lost(Some(4)), checked as typed Reason::Closed BEFORE
local cancellation. Canonical cause and authoritative Finished90/90 remained
unchanged. Producer capacity stayed0. One valid42B postterminal payload returned
typed Closed with the same allocation/capacity/exact bytes; original/returned
binary files compare equal, SHA256
242ddcff2ed0a63c363827b6bd28859e31fa951b483b0739099d59e97c164fb8.

Observer stop/join completed with no future Record and owner control still
Finished90/90. Owner abort retained90/90 in remote Aborted; finish returned
Err(Closed). The endpoint worker completed/joined; cleanup errors were empty,
protocol_complete/cleanup_completed true, FD4→4. Duration0.08s is a finite-test
receipt, not a latency benchmark. Transport drain, full population/kernel return
and daemon metadata/durability remain Unknown/unverified.

Local finish/pending0 and peer processing have separate actual witnesses.
Empty current pending is not an Io::write return0 or whole-stream zero-byte/
final-size claim. FIN-RECEIPT-AUTHORITY.md records the source/primary distinctions.

## Build and focused checks

Private Fedora8fadc, Linux6.19.10-300.fc44.x86_64/glibc2.43; Rust1.98.1
(48a229cea, LLVM22.1.8), Cargo1.98.1, Zig0.16.0. Admission2CPU/4GiB/24GiB/j4
does not establish measured physical/parent/kernel or hard-cgroup peak bounds.

r163 normal carried init/server/VM/net caller build passed0 without skip,
prebuilt/helper or ABI overrides. Exact command:
`cargo +1.98.1 test --locked -p shards --test managed_output --profile test-release --no-run -j4 --message-format=json`.
Cargo reported8m02s; total584.45s including HTTPS checkout/setup, within the
original3600s bound. No security/trust setting changed. Frozen artifacts:

| Artifact | SHA256 |
| --- | --- |
| managed_output, fresh:false | add12067635b7cec14c09885c9222fdc581f6958a549fa0e307ac468ed0c7014 |
| normal carried shards | a529ccad06afd53f8b479a83970158f96455388a34027c7a705a1d2b38850c4c |

The native invocation explicitly selected one ignored entrypoint with
--ignored/--exact/--test-threads=1:240s work+15s cleanup/direct255s outer.
No SKIP. r168 pure checks passed7/0failed/4explicitignored; ignored actors are not
counted as pure passes. The one approved native selection ran separately above;
other actors remain unrun here. Existing pure tests include Writes::new(0)→Empty
and strict positive/drain/overflow checks. Strict fullcaller Clippy passed0 in
1m36s with -D warnings. No VM/mutation executed.

Canonical named rustfmt--check, source diff check and development-policy pass0.
preserved-assertions.json verifies the original positive/FIN accountant is
byte-identical after only ignoring the added enum field. Exact source/shim
readback preceded build/actor; all six source/dependency hashes and the compiled
native leaf fingerprint matched before/after execution.

## Preserved headroom hold and raw proof

The first native admission was refused with0 idle cores below the required2.
It dispatched no command/process/endpoint, made no case directory and started
no case deadline. A later receipt reported15 idle cores/54GB free; the UNCHANGED
2CPU/4GiB/24GiB request was then admitted and the actual case ran ONCE.
No guard bypass, resource reduction, reset or test retry. native-admission-held,
headroom-snapshot and native-admission-restored JSON retain both observations.
The refusal is not a test or automatic-review failure. Historical4763/44414
checkpoints retain their then-pending status; no original raw was overwritten.

evidence-empty-fin-v1.tar.gz is32428B, SHA256
342b61e079283207520ba388389fd838a206b656074304271b0c3d1bb301dd9c.
Exact inventory:65 original payloads + member-hashes.json =66 total members.
All66 paths are unique safe regular files; every65 original hash was verified
locally. Outside manifest, fixture/CLI receipts and validated-case.json are
byte-identical archive members. Collection found no validation errors.
Original commands/environment/stdout/stderr/exits, ABI/returned bytes, typed
events and credit/terminal/stop/join/FD facts remain on the private runner.

## Operations and owed ROOT gates

All private source/target/Cargo/Zig/runtime caches, artifacts and records are
under /home/adalundhe/managed-empty-fin-0342-v1. Frozen source ref
frozen/empty-fin-d8a2 retains the actual source. No owned job is active; no LOCAL
target/cache, new local worktree or active cache deletion. Old51a/82019/4ef
source/raw/failures/frozen artifacts, ROOT caches and paused WinARM are untouched.

This finite case/light batch is returned; the lane is not released. ROOT owes
latest combined consumer composition, meaningful guard mutations/restoration,
supported seven-target/real-hypervisor matrices and broader disconnect/revoke/
order/resource/durability gates. No full management/P0–P13/G25, allocation/
performance or whole-system cleanup claim follows from this case.

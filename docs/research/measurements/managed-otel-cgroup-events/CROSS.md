# Independent ARM Mac cross-validation

This is the explicitly authorized static follow-up to signed
67a0cdadee0479c0998041b84f77df344730d24d. It does not replace native Linux unit
tests, actual Reader behavior, official-reference execution or ROOT's integration
gates. Production, fixture, reference and profile-input Rust source is unchanged.

POLICY_READ implementer at current mainca3ab10c151aa5539e757f1cc76029746ccbb561;
FULL required policy/plan/490-line updated handoff/0ae read; rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Receiptseq7847. Owned additions are only run-cross.py and this directory's new
cross evidence. Source limits, native initial failures, offline refusals and all
prior source/raw/frozen artifacts remain intact. Fedora home is read-only and
ROOT caches/targets, paused WinARM and automatic-review holds are untouched.

## Exact input and requested gates

The SAME corrected32 source payloads plus embedded manifest are supplied as
[light-v2/source.tar.gz](records/light-v2/source.tar.gz),91262bytes,
SHA256a2fdebd9f18bc002cb521eb00cbbaca77a61b474332ad18e317beda92a8d486a.
The product lock is45993bytes,
SHA2561ecb41e738c4c4133a1968873e424efce5b2a65899dd8f40f37ac6aaffec8133,
byte-identical to current mainca3. No dependency update, policy waiver from the
older embedded source record, source formatting write or package substitution.
The separately supplied driver is8372bytes,
SHA25603242bce9a3031bddd3683ad52bcdf52c9eebff3c7ba69efea836f42b0570a77.

Macb24c5c951260dcdfd0972f2aeaf3cf51, ARM64; NEW exclusive
~/managed-otel-events-cross-67a0-b24-v1/{source,target,cargo-home,tmp,out}.
Normal Kiln2CPU/2GiB/4GiB,600s,j2 admission; lockf on /tmp/g121-mac.lock and
actual30GiB floor. Preflight also leaves the full admitted4GiB allowance above
that floor; no cache deletion or larger resource envelope. Only already installed
Rust1.98.1 and GNU/musl standard libraries may be used. Repository rust-version
is MSRV, not a toolchain pin. Current inventory/launch/policy/source records are
under [cross-mac-v1](records/cross-mac-v1).

| Gate | Exact static scope |
| --- | --- |
| Formatter | rustfmt --check only, frozen Reader/module tests, shared fixture/reference and profile input source |
| Reader+codec GNU | cargo clippy --locked --all-targets --target x86_64-unknown-linux-gnu -j2 -- -D warnings |
| Reader+codec musl | Same command for x86_64-unknown-linux-musl |
| Encoder | Same strict static check, GNU, existing standalone fixture |
| Official Rust reference | Same strict static check, GNU, including complete-equivalence test source without invocation |
| Profile default / alloc-count | cargo check --locked, GNU, each configuration; no executable invocation |

The driver invokes toolchain version queries and these static commands. Normal
Cargo host build-script/proc-macro execution is part of compilation; no project
fixture, unit-test, integration-test, encoder mode or profile executable is
invoked. Compiled assertions are not executed assertions. No VM, native cgroup
read/write, pressure, Collector, mutation, profile or benchmark occurs.

## Result and retained limits

The actual request completed0, with all13 commands0: six tool/prerequisite
queries, formatter check, four strict static Cargo gates and two profile static
configuration checks. Every requested gate in the table passed in this narrow
cross scope. Actual installed tools: rustc1.98.1/48a229ceaefd4985c50990b14116b6d856af0985,
Cargo1.98.1/797e8a9bc, rustfmt1.9.0 and Clippy0.1.98, LLVM22.1.8.
Their complete version outputs and binary hashes are retained, together with13
Rust metadata artifact identities; these artifacts are not executed programs.

Raw archive8168bytes,
SHA256bb5a176fe3a891f6bce0d6c86716fab99fdf12d81c1704750c3a4318fa4a7e88:
EVERY40 original payloads plus one embedded manifest independently checked as
unique safe relative regular files with exact lengths/hashes. Inventory41 rows
distinguishes payloads from the manifest. [Verification](records/cross-mac-v1/verified.json),
[all original outputs](records/cross-mac-v1/raw),
[source/tool/metadata identities](records/cross-mac-v1/artifact-identities.json).
Original source files remain byte-identical. The actual floor witnesses are
81266143232bytes before and80948748288bytes after, both above30GiB. These are
non-atomic free-space guard observations, not attributed build costs or a
physical memory/disk-peak guarantee. The lock child returned0; all new private
source/cache/metadata/output files remain preserved, with no active own job.

One original bootstrap metadata file, outside the raw archive's40 payloads,
appends literal backslash-n rather than LF. Its JSON-body parse initially reports
extra data. [Original bytes](records/cross-mac-v1/lock-command.original.json) are
unchanged; a separately labelled [exact-body association](records/cross-mac-v1/lock-command.association.json)
records this mechanical suffix and original lock argv/exit. No source, compile
or gate rerun followed this metadata error. It is not silently rewritten as a
clean original file.

Original native Linux v2 remains unstarted after the recorded offline admission
refusal. All native Linux tests and official Rust decoded-equivalence execution
remain UNRUN. Cross0 does not close parser/syscall/wire/runtime correctness or
make this slice ready for production adoption.

R01/R11 static type/lint proof, if successful, remains narrower than actual Linux
syscall/parse/wire behavior. R02/R03/R04–R07/R12/R13 source contracts and literal
derivations remain in PROPOSAL/D229/EVIDENCE. R08/R14 performance/parent/kernel
costs remain ROOT-owned and unmeasured for this source. R09/R10 retain the
original fmt failure and refusal rather than manufacturing a clean history.
R15 no children/goals/main merge or readiness claim; ROOT still owns actual
caller, kernel/reset, official consumers, seven-target/VM and adversarial gates.
R16 exact new private paths, actual floor/lock/admission and original artifacts
are preserved; cross-validation introduces no cleanup authority.

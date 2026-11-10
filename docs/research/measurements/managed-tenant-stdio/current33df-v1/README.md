# Fresh lifecycle checks after the core landing

Assigned adversarial agent `01a1c5d28cea74a5b0241200190d6c54`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Policy read against main
`33df95b08ad8f95bddeaa1c48acf0c64386e0eac`, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.

This finite campaign builds and runs the existing normal-exit and actor-stdio-EOF
fixtures against the landed core. Its candidate adds evidence/commands only;
production and test source must match main33df (the code/test bytes also match
the coordinator's gated0219 batch). Earlier live0ea results remain historical.
No inherited-child task seam, observer/client API or production behavior changes.

## Scope and contract

- Runner: Fedora `8fadc1ec213332e16cb0d915a3fb913f`, Linux x86_64 KVM.
- Exclusive private scope:
  `~/managed-tenant-current-33df-01a1c5d2-v1/{source,target,records,cache,frozen}`;
  short private socket directory `/tmp/mt-01a1-current33df-v1`.
- Full request: 4 CPU, 8 GiB memory, 40 GiB disk, including fresh source, normal
  CLI/carried init/server/VM/net builds, static testguest, test binary, VM and logs.
  Kiln admission precedes any remote action; filesystem/KVM checks precede build.
  No ROOT cache or earlier campaign target is borrowed.
- Build bounds: 3,600 s host/carried build, 1,800 s static guest; TERM then 30 s
  kill guard. Each real case has an outer 900 s bound; explicit original work and
  cleanup inputs are 300,000 ms each. These are caller campaign limits, not product
  constants or a performance acceptance threshold.
- Explicit guest ports come from two distinct OS-assigned loopback socket binds;
  the existing checked NETWORK/EXPOSE/CONNECT declarations remain unchanged.
  Existing `-p127.0.0.1::{guest}` supplies actual OS-assigned host publication.
- Stdio challenge is a fresh UUID's 16 bytes. Exact native capture is 49 bytes;
  request/reply frames each contain 1 tag + 4 length + 16 challenge bytes, hence
  the explicit 91-byte report envelope. No aggregate resource guarantee follows.
- Exactly one selected ignored test must actually execute per case. Both actor
  channels require canonical End, ACK and transport EOF; later sibling fd1/fd2
  and main bytes remain causal assertions. Stdio additionally requires a fresh
  actor response after both output EOFs and a live actor row.
- Final actor metadata must remain `ended/complete`, with exact accepted/consumed
  wire counts. Continuing sibling metadata remains `aborted/unknown` after stop.
  Owner stop, observer joins, stopped PID0, removal and host FD baseline are
  checked. Full population reaping, kernel/parent resource return remain Unknown.
- Failures and timeout cleanup uncertainty are retained before any next case.
  No assertion waiver, hidden retry, production fix or rejected campaign follows.

The optional task seam needed to compose inherited-writer A on current main has
one new ROOT-owned broker caller. That separate ownership dependency is reported;
this independent normal/stdio baseline needs no adaptation. Earlier sources,
signed checkpoints and frozen binaries stay unchanged. Reconnect Step2 source/
light acceptance is preserved at `5feb55e135fdd276eccb4386d5f061553d3902ca`;
actual reconnect acceptance remains a coordinator obligation.

## Results and limits

Exact signed tested source: `4d80f06048fbd11477032ae9b1bb95b02ae3171c`.
The source-code diff against main33df is empty. GNU host rustc1.98.1/LLVM22.1.8,
Fedora44/kernel6.19.10, KVM API12, uid1000; carried init/server and testguest use
the musl target. This closing evidence changes no source/test bytes.

| Actual stage | Result |
| --- | --- |
| r5 initial private preparation | 127 before compilation: `rg` is absent on Fedora. Clone, policy/kernel identity and toolchain records retained; no actor/VM started. |
| r7 same-source resumed build | Full host/normal carried init/server/VM/net compile0, static testguest0, seven-artifact index/freeze0. `resume-build.command` replaces only the orchestration target-list check with Python; no source edit/rebase. |
| r12 normal exit | Exactly1 test passed,0failed/ignored,20filtered. Both actor canonical End/ACK/transport EOF, later exact sibling fd1/fd2 and main progress; strict final metadata and cleanup passed. |
| r12 actor stdio EOF | Exactly1 test passed,0failed/ignored,20filtered. Both channel EOFs precede the fresh16-byte challenge; actual21-byte request==reply plus peer EOF, visible non-zombie actor row, subsequent sibling/main bytes; strict metadata/cleanup passed. |

Both final metadata files retain actor `ended/complete` at accepted==consumed
102/99, and continuing sibling `aborted/unknown` at177/171 after owner stop.
Both cases completed protocol, stopped PID0 inspection, removal, owner stop and
observer joins; both host FD observations are11→11. No timeout occurred. These
are owned observations, not independent full-population wait/reaping, parent/
kernel resource accounting, leak freedom or a general lifetime proof.

The exact [raw record](raw-v1/TRANSFER.json) verifies a23,797-byte archive,
SHA256 `319a69498057a145af103909c288043bd8346f3e24a9f496870ea290f681f3c5`,
two <=12,000-byte chunks and92 safe regular archive members. All91 original file
sizes/hashes were verified locally; logs retain exact bytes under reversible gzip,
with original/stored mappings in [STORAGE.json](raw-v1/STORAGE.json). Full Cargo
JSONL (278,255bytes, SHA12eb327c...) remains in the remote owned records. Source,
targets, cache and all seven frozen binaries remain intact. No raw whitespace was
normalized or waived. Source411,588KiB, target2,104,872KiB, frozen369,756KiB and
cache19,620KiB are build-footprint observations, not resource admission proofs.
Full artifact hashes are in [artifacts.json](raw-v1/artifacts.json).

The next finite case uses this same frozen source/binary and existing fixture:
minimum positive challenge count1 and derived report61 bytes. Its command is
prepared, not counted as run at this checkpoint. A's exact existing composition
is retained as the lossless `inherited-current-proposed.patch.gz`, original
SHA256 `7babf8cba357f001b41106ac26ebefd16845ea75a141e085ed8b36e966e2b777`.
Apply-check0 covers the current33df source shape; it is unapplied and has no
current full compile/live pass. Its sole extra ROOT-owned broker caller adds one
`None` so both optional inputs remain absent; that caller authorization is owed.
No production defect/fix or current mutation guard claim is inferred.

Coordinator review/integration, supported other VM hosts/seven-target gates,
actual reconnect and full accounting/ownership/resource gates remain owed.

R01/R04/R06/R07/R13: source unchanged, existing explicit fixture transitions and
strict assertions retained; script checks are test assertions. R02/R03/R05/R12:
reuse the approved native fixtures and current APIs without a new mechanism,
cache, polling, manager or grant. R08/R14: no audit/profile/benchmark or 4–5x,
leak-free or hard aggregate claim; coordinator comparisons remain owed.
R09/R10: this is a fresh revision-specific regression baseline, not a bug fix;
all observed failures and exact commands will be retained. R11: only actual
Linux GNU x86_64 KVM with musl guest/helpers is attempted; other hosts/VMs and
seven-target integration are unrun here. R15: bounded assigned campaign only,
no children/goals/main merge; coordinator evidence review and integration owed.
R16: existing Mac worktree, no local builds/full copies, Mac floor checked;
new private admitted Linux paths only, source/evidence/frozen artifacts preserved.

Full P0–P13/G01–G24 and this continuing lane remain open.

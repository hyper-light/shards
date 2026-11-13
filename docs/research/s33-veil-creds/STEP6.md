# Step 33, step 6: Hex fills a harness VM's grants and answers its leases from kiln's broker (D242)

Kiln goal #70 (8f474b45c7cd86559e0566f22f9d0c33), plan step 33 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d; step 6 of
the goal's plan 01a1d2b88be57304b972761e2a5c6781 (research 01a1d2b88be57304bf6b081fe46832e0: f5, f76; f3, f7, f72).
Role: implementer. Policy read (POLICY_READ sent to ROOT before any edit):

| Repo | Revision | File | sha256 |
|---|---|---|---|
| Shards | origin/main 95283d540 | AGENTS.md | f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41 |
| | | docs/development-rules.md | df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3 |
| | | docs/development-evidence.md | 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63 |
| Hex | origin/main 025aae6d3, and the lead's main cf535152 | CLAUDE.md (AGENTS.md empty) | 89f7f8e8041423b2561e9fbe0617eba0a0c5a20f7966f792c6bbd1c062c135fe |
| kiln | origin/main fa3411ea4 | AGENTS.md | 16b05ef80db024a7b54b0e703d42c8cf5c1f330a626d6bd85ae4525609447a4c |
| | | CLAUDE.md (`@AGENTS.md`) | 336cc4fbf19beaada7ccf9986414fa91851a8d7a07dfb3ccbe800a69eed0ab49 |
| | | docs/ci.md (§0, lines 59-219) | 6ca5559a… (§0 1e464890a193c0d1f4c4a4e88d47a8beb0497e0ef39ba19b0a189963f9bd1201) |

## What landed, and where

| Repo | Commit | What |
|---|---|---|
| kiln | 97ec16924 | kiln-seams: a Sandbox launch's credentials by reference (`CredentialGrant {reference, toward}`, appended); `Holds::credentials`; the check (a store's name, a DNS host without a port, one its egress reaches; else `Invalid` before anything unheld); the simulator's `fill`; SANDBOX-13; kiln's host sandbox holds none; an isolated launch grants none yet; docs/seams.md |
| kiln | 802a78b13 | the broker's `Leased::lease` and `release_lease(key, lease)` (one lease given back alone); `secrets::lessor` (`Lessors`, `RunLessor`: a run's identity its key where it binds one, else a channel of 256 random bits; lease, release, ended), mirroring Hex's lease seam until kiln vendors it |
| kiln | e76cc5e72 | kiln main at the landing (a merge of kiln main 21dbe546d, which touched a test of isolated.rs alone) |
| Hex | 4643fe06 (red), 9ac306be (green) on the lead's main cf535152 | local branch `p/s33-hex-v1` (`/Users/adalundhe/Projects/hex/.claude/worktrees/s33-hex`), handed to the Hex lead to merge; nothing pushed. hex-api `credentials.rs`, `lease.rs` (+ `sim`, `conformance`), `ShardsImage::credentials`, `BoxInfo::credentials`, `Refusal::Unconfigured`, HEX-IMG-8; hex-core's wire (credentials, tag 15, IDENTITY); hex-runtime `guest/leases.rs` and image.rs's create, image_spec and start; docs/design/credential-grants.md |

The design is Hex's docs/design/credential-grants.md (C1-C6) and D242's "Built: step 6". In short: the grant names who
(a member), which credential (a reference) and toward which hosts, out; one check holds it to its member's own egress
(by name, by TCP), refuses what could never be filled in as it says, and refuses a host another member reaches
ungranted (the injector ends TLS for every member reaching a granted host); Hex lowers the box's grants, adding none;
its node is given its orchestrator's `Lessors` and gives each run's boot a `Lessor` of its own, answering Shards' run
host's asks and passing on each lease Shards' session gives back, once, by whatever end.

## Tests: one host per repo, adacachy2 (Linux x86_64), rust 1.99.1

The carrier, `.claude/worktrees/s33h-carrier` in Shards (`kiln.sh`, `hex.sh`, `hex-final.sh`), clones each repo
blobless on the runner, fetches its bundle (Hex's holds the lead's local main) and runs at nice 10 with a cold target of
its own (`~/g121-tmp/s33-hex-kiln`, `~/g121-tmp/s33-hex-hex`, removed after).

| Run | Repo, tree | Command | Result |
|---|---|---|---|
| r426 | kiln 802a78b13 | `cargo test -p kiln-seams --lib sandbox`; `cargo test -p kiln-seams --test doc_truth`; `cargo test -p kiln --lib -- secrets:: sandbox::`; `cargo clippy -p kiln-seams -p kiln --all-targets -- -D warnings` | 7/7; 3/3; 34/34 (each lessor and broker test over SimSecrets and kiln's store); clippy clean |
| r503 | Hex red 4643fe06, then green 4632b83c | `cargo test -p hex-api -p hex-core -p hex-runtime --locked --no-fail-fast -- w33 sim:: differential wire::tests record guest::image guest::leases grants::` at each; two guards removed on green; `cargo clippy -p hex-api -p hex-core -p hex-runtime -p hex-node -p hex-client --all-targets --locked --target x86_64-unknown-linux-gnu --target aarch64-apple-darwin --target x86_64-pc-windows-msvc -- -D warnings` | red: hex-api 6, hex-core 12, hex-runtime 5 failing; green: 33/33, 131/131 (2 ignored), 64/64; guards below; clippy clean on the three OSes |

The green handed over, 9ac306be, differs from the tested 4632b83c in docs/design/credential-grants.md alone. The
earlier runs r440 (Hex on a scratch merge of the lead's branches) and the first r386 (kiln, before two compile errors
were fixed) are superseded.

### Guards removed by hand, each failing its test (R10)

| Guard removed (green) | Its test | Result |
|---|---|---|
| The record: inspect's credential grants dropped (image.rs `info`) | `w33_a_harness_vms_credentials_reach_its_boot_and_record_and_lease_from_its_lessors` | red, at its inspect assertion |
| The release: a lease Shards gives back not passed on (`RunLeases::release`) | the same | red, "the run's lease outlived its end" |
| Every guard at once (the red commit: the check, the wire, the lowering, the lessor, the release, the record, the simulator's give-back) | every w33 test, HEX-IMG-8, HEX-LEASE-3, the refusals' round trips | red (r503) |

## Owed

- **The Hex lead:** the merge of `p/s33-hex-v1`, its gates, VMs and conformance on Linux, macOS and Windows, and the
  re-vendor past Shards 740fa372, whose step 3 gave the grant its member's placeholder: Hex's grant carries it then
  (kiln mints it) and its one-credential-per-host rule goes.
- **kiln, after kiln vendors that Hex:** `hex_api::lease::Lessors` implemented over `secrets::lessor`, given to the node
  its harness microVMs run on, and its sandbox's grants carried to `ShardsImage::credentials`; the lease contract
  (HEX-LEASE-1..4) run over SimSecrets and kiln's store.
- **A remote node:** one reached over its wire is given no lessors and refuses grants `Unconfigured`.
- **ROOT:** CI on kiln main; step 10's real runs (a granted provider's credential filled in at egress on KVM, HVF and
  WHP, an ungranted host passed bare, the lease given back at the run's end, no value in the VM).

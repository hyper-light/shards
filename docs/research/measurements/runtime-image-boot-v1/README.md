# runtime-image-boot-v1: evidence (implementer record)

D231, [decision record](../../runtime-image-boot-decision-v1.md). Implementer's light checks
only: nothing here ran on a VM, and nothing here is a performance claim (R14 is ROOT's).

## Identity

- Policy: origin/main bff3015182ad754274f1048ce393ac12cb6cd91f, docs/development-rules.md
  sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3
  ([receipt](POLICY_READ.json)); role implementer; coordinator ROOT
  01a148884a4770fdaf0111f3f792a248.
- Branch `p/runtime-image-boot-v1` from `origin/p/integrate-engine-20261111` 82083f27,
  with the integration branch's ring window fix 032b63b1c merged at ROOT's word (75c36864f).
- Checks ran on kiln runners `kiln-a094a12d.attlocal.net` (8fadc1ec…, Linux x86_64, under
  `systemd-run --user --scope -p MemoryMax=24G`, `-j4`, no amphora-bench running) and
  `adacachy` (451397fc…, Linux x86_64, `MemoryMax=16G`, `-j4`) when the first stopped
  answering its sync. The shards crate was checked with `SHARDS_HELPERS=skip` and
  placeholder `SHARDS_INIT_BINARY`/`SHARDS_SERVER_BINARY` files: what it embeds is not built
  or run by these checks.

## Light checks (Linux x86_64 gnu)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean after one module-order fix (main.rs) |
| `cargo clippy -p shards-abi -p shards-api -p shards-ipc -p shards-init -p shards-testguest --all-targets -- -D warnings` | clean |
| `cargo clippy -p shards --all-targets -- -D warnings` (lib, bin, tests incl. `tests/api_image.rs`) | clean |
| `cargo test -p shards-abi --lib` | 37 passed (9 new: `run::members::tests`) |
| `cargo test -p shards-api --lib image` | 9 passed (new: `image::tests`, `image::wire::tests`) |
| `cargo test -p shards-ipc --lib every_message_kind` | passed (`MEMBER_ENDED` 87 unique) |
| `cargo test -p shards-init -- staged stats_scope` | 7 passed (new: `staged::tests`, `stats_scope_tests`) |
| `cargo test -p shards --bin shards api_run` | 4 passed (new: `api_run::tests`) |
| `cargo test -p shards-api --lib` (whole) | 58 passed, 3 failed in the pre-existing `taken::linux` allocation tests; attribution in "Open" below |

Not run by this implementer (owed to ROOT, §8 of the record): the seven-target lint
(`scripts/lint`), any guest build for musl, every real-VM test, any Windows or macOS build.

## Mutations (R10): each guard removed, its test fails for that reason

Script: `mutate.py` beside the gate (applies one change, runs the one test, restores the
file and touches it so cargo rebuilds), adacachy, after the gate above.

| Mutant | Guard removed | Test | Result |
|---|---|---|---|
| M1 | abi `variable_ok`: `SHARDS_DOMAIN` refused | `run::members::tests::variables_are_checked` | killed |
| M2 | abi `path_ok`: `..` component refused | `run::members::tests::paths_beneath_the_home_alone` | killed |
| M3 | init `staged::write`: `O_NOFOLLOW` on a directory | `staged::tests::links_in_the_way_are_not_followed` | killed |
| M4 | init `staged::check`: a member that runs nothing refused | `staged::tests::staging_into_no_running_member_is_refused` | killed |
| M5 | init `stats_cgroup`: a joiner's exec reads its own cgroup alone | `stats_scope_tests` | killed |
| M6 | api wire `Reader::end`: trailing bytes refused | `image::wire::tests::a_cut_or_padded_request_is_refused` | killed |
| M7 | api `request`: a variable's key holding `=` refused | `image::tests::what_the_guest_refuses_is_refused_first` | killed |
| M8 | run host `kept`: a grant the image does not give refused | `api_run::tests::grants_narrow_and_never_widen` | killed |
| M9 | run host `ports_granted`: no ingress receiver, no command, refused | `api_run::tests::ports_need_a_receiver_without_a_command` | killed |

These are unit-level guards. The forbidden effects they stand for (a widened grant reaching
the network process, a staged file landing outside a member's home, a member's credential
reaching another) are owed as real-VM observations (§8 of the record).

## R01–R16 accounting (implementer's part)

- R01: no unwrap/expect/panic/indexing in production code (clippy's denials clean); checked
  arithmetic on untrusted sizes (wire counts bounded by the bytes left, `checked_add` for
  frame lengths); poison-tolerant locks; threads by `Builder`.
- R02/R03: decision record with sources and alternatives; no new runtime literal (bounds are
  `run::MAX_PAYLOAD`, `run::CHUNK`, `run::TOKEN`, Linux's `PATH_MAX`/`NAME_MAX`, the
  permission bits); every wait on a fact.
- R04: states and refusals typed (`ImageRefusal`, `wire::Cause`, `RunEnd`, `MemberEnd`,
  `StreamEnd`); unknown tags and versions refused; `Secret` never in Debug.
- R05/R13: one new interface (`Images`/`ImageGuest`) at the existing api seam; a simulator
  for it is owed.
- R09: no defect is fixed here; this is new behaviour. The three `taken::linux` failures are
  not attributed until reproduced (below).
- R11/R12: Linux and macOS only for the run host in v1; Windows refused `Unsupported` (owed,
  not a platform limit); the run host's launch is a third copy of the builder's and the
  daemon's (owed, record §3.5).
- R14/R15/R16: no profiling or benchmarks run; no subagents; runner caches left in place.

## Open

- `taken::linux` (crates/api, pre-existing tests from
  docs/research/measurements/managed-proc-main/reader-tests.rs) failed 3 of 61 in a whole
  `shards-api --lib` run on adacachy, a host under load from other work. Their refusal hook
  is thread-local, so another test's allocations cannot trip it, but they compare
  `/proc/self/fd` counts before and after, which another test thread's descriptors change.
  The reproduction (alone, serially, and on the base revision) is recorded in the report.

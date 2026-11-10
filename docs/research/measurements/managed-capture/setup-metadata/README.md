# Binary managed metadata and workload setup

Coordinator `/root`, task G01/P3 combined validation. POLICY_READ c921a0b1,
SHA256 `1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Owned change: `init::setup::apply`, its focused test and this evidence directory.
Fedora x86_64 KVM runner8fadc; rootless host uid1000; Rust1.98.1 pinned by the
repository. Isolated `~/managed-capture-check`, target `~/managed-capture-target`;
request4CPUs/8GiB/20GiB, cargo -j4. Credentials and Docker configuration were not read;
a separate empty DOCKER_CONFIG was used.

## Actual failure and correction

Real KVM run r89 at `6d7cc93c` failed0/1, exit101 in23.33s before actor launch.
The generic setup message said “error remounting the root read-only: invalid
argument”. The failing entry was not a mount: the new authenticated capture token
was arbitrary binary in Spec.setup, while setup::apply converted every entry not
already classified as init-owned to UTF-8. Managed activation also has binary IDs.
The parent capture consumer owns/checks this metadata; workload mount setup does not.

Fixture `887d4e76` supplies a valid all-FF token and valid active/member/capacity
entries through the actual handler. Native r91 failed0/1 with Err22. The earlier
r90 command selected --lib in a binary-only package; its preserved failure is an
orchestration error and provides no product reproduction.

Fix `ba248442` classifies the four canonical ABI keys with the existing init-owned
entries before UTF-8 decoding. It preserves the actual binary representation,
parent capture validation and rejection of unrelated invalid UTF-8. It changes no
mount flags, seccomp rules, grants or token matching. R92 first passed1/1; independently
removing only those eight lines failed0/1 with the same Err22; restoring passed both
setup tests2/2. Strict native Linux init clippy passed. The exact original real-VM
test then passed1/1 in3.89s, exit0: main JSON-RPC stdout and main stderr were exact,
with only its explicitly checked final host timing diagnostic. This does not prove
subscriber authorization, network interception or full per-channel output delivery.

## Design and rule review

R01/R04: early classification performs slice comparisons without allocation,
indexing or unchecked arithmetic; it returns the existing Result, not a new string
state protocol. Typed ABI validation remains in the manager. Existing setup String
presentation and its misleading unknown-entry fallback are not redesigned here.
R02/R05/R12/R13: the established local pattern already excludes binary seccomp and
init-owned domain entries. Reusing that seam is smaller than changing Spec's wire
layout or hex-encoding tokens to satisfy an unrelated parser. No new architecture,
platform branch, trait, manager, cache or periodic wait is introduced. Primary local
contract: abi::run::managed key definitions and checked token/active/capacity readers;
init::capture::Capture::prepare owns their use. Existing broader capture decisions
and alternatives remain in ../core.md and ../wire.md.
R03: no production literal or capacity changes; fixtures derive token/ID/frame sizes
from the ABI. R06/R07: one flat condition, no extra state or explanatory comment
layer. R08/R14: no performance/allocation/fault or competing-system claim; those
combined gates remain open. R09/R10: original live failure, minimized red, green,
independent guard removal and restored original VM test are retained below.
R11: actual GNU/Linux native+KVM only; supported seven-target and other VM hosts
remain unrun. R15: coordinator owns this composition/fix; other agents continue
independently. R16: no cleanup or user data changes; private targets only.

The later registry composition `d414d21d` has not inherited a passing exact-tip VM
gate merely from this ba248 result. Closing integration/matrix tests remain owed.

## Repeat

Run the following only in an isolated checkout with the stated runner envelope:

```sh
export CARGO_TARGET_DIR="$HOME/managed-capture-target"
cargo test --locked -j4 -p shards-init --bin shards-init \
  setup::tests::managed_capture_metadata_is_not_a_workload_mount -- --exact --nocapture
cargo test --locked -j4 -p shards-init --bin shards-init setup::tests -- --nocapture
cargo clippy --locked -j4 -p shards-init --all-targets -- -D warnings
cargo test --locked -j4 -p shards --profile test-release --test build \
  supplied_member_output_does_not_rewrite_the_main_protocol -- --exact --nocapture
```

For the independent mutation, reverse `managed-combined-ba248-setup-and-output.guard.patch`
against ba248, run the first command and require the actual Err22 assertion;
restore the patch and rerun. Keep every output under a new prefix. Restoring only
this file to the exact source is mandatory before further checks.

Raw files in this directory retain full logs, statuses and the exact guard patch.
The original r89/r91 and r92 tool transcripts also retain commands and outcomes.

# Revalidate managed activation from the opened image

Coordinator `/root`; source84824ea1282097514ca664298afe1aa10170b82a on endpoint
prototype b6866be5. Canonical policy c921a0b1, SHA-256
1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d.

The live file-backed EROFS fixture writes a valid Agentfile and runnable member,
reads it, replaces that image at the same pathname with a files-only member while
keeping the normalized Agentfile and its digest unchanged, then reads again.
The cached map returned the first active-member list. This proves a stale result
at this API seam; it does not prove that an actor can replace a production image.

```sh
SHARDS_HELPERS=skip cargo test --locked -j 4 -p shards --bin shards \
  agentfile::tests::runnable_members_are_read_again_when_the_image_at_a_path_changes \
  -- --exact --nocapture
```

| Run/source | Actual result |
| --- | --- |
| Fedora r83/27c3bdb3, cache present | Exit101,0passed/1failed at active_members.is_empty, not a setup/compile failure. |
| r85/84824ea1, cache removed | Exit0,4Agentfile tests passed/0failed, including sparse IDs and both legal symlink forms. |
| r85, retained restore-cache-mutation.patch | Exit101,0passed/1failed at the same stale-members assertion. |
| r85, canonical source restored | Exit0,4passed/0failed. Strict shards binary clippy exit0. |

Runner8fadc1ec is Fedora amd64, Rust1.98.1. Source hashes and final log excerpts
are retained beside this file; full logs remain in the runner's home. The unit
fixture uses real files and the production reader, not a mocked cache. No VM,
provider call, compromised-image exploit or performance result is claimed.

The correction removes the process-wide map, key allocation, value clone and
global lookup lock. Each lookup opens the actual image and checks its normalized
Agentfile digest before deriving the current plan. No new cache, invalidation
watch, timer, tuning literal or abstraction replaces it. Existing per-run owned
plan values remain necessary input to admission. A pathname is not itself an
immutable file identity; [open's file-description semantics](https://man7.org/linux/man-pages/man2/open.2.html)
also distinguish an opened object from its later name resolution.

Alternatives considered: adding metadata/content identity and invalidation to the
map would retain another derived store and its resource/lifetime obligations;
hashing all referenced data would add I/O before knowing a hit is valid. Neither
has a demonstrated benefit here. The separate host whole-tree active classifier
is still an experimental cost/duplication issue. Reusing trusted init's existing
pre-release runnable facts through checked activation/ACK is a candidate to remove
that scan and duplicate parsing; it has not been implemented or measured by this
cache deletion. Do not use this fix as justification for that classifier's cost.

R01/R04/R06/R07/R13: production deletes state/locking/cloning rather than adding
failure paths; remaining file/parse errors retain the existing outer String API,
with no string-based control decision. Broader fallible-allocation review of the
existing readers remains open. R02/R03/R05/R09/R10: actual red/green/restoration
mutation, source/API comparison and no new runtime literals are recorded above.
R08/R14: no latency, retained-memory, allocations/faults or4–5x claim; comparable
profile/measurement work remains required. R11/R12: shared Rust reader unchanged
across platforms; only native Linux amd64 ran here. Seven-target checks, Windows/
macOS native behavior and real-VM combined gates remain owed. R15/R16: root owns
this implementation/evidence; separate private compiler cache only, no active
cache/source/evidence removal. Other implementing agents were preserved.

The repeat script is the exact scoped r85 command. Run only in an isolated
checkout after supplying the expected source and a private CARGO_TARGET_DIR.
It refuses to overwrite evidence; use fresh output paths for another run.

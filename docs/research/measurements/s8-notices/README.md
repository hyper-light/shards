# Step 8, part 2: names of another case on case-insensitive hosts, and host edits waking the guest's inotify (D235)

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 8: "Foo and foo are handled as the host
does; a host edit wakes an inotify watch in the guest". Part 1 (projects plugged into a
running member) is [s8-project-paths](../s8-project-paths/README.md). The decisions and their
alternatives are architecture.md D235's; this is the implementer's record.

## Identity

| | |
|---|---|
| Policy | `origin/main` `fc71859bc`; AGENTS.md sha256 `f66d304c…6db41`, `docs/development-rules.md` `df2acf47…d4d3`, `docs/development-evidence.md` `61613a24…fb63`; role implementer (readback sent to ROOT, session g121-a0) |
| Source | branch `p/s8-part2-v1`, from `origin/main` `fc71859bc` |
| Hosts | adacachy (Linux 7.2.9-2-cachyos, x86_64, 32 CPUs, KVM); adas-mac-studio.local-2 (macOS 26.4 25E246, arm64, APFS case-insensitive, HVF); the Windows 11 guest on adacachy (x86_64, NTFS, no hypervisor) |
| Toolchain | Rust 1.98.1 (`rust-toolchain.toml`), zig 0.16.0 (`scripts/install-zig`) |
| Owned paths | vmm `devices/virtio/fs/{server,server_unix,server_windows,notices,mod}.rs`, vmm `platform/unix.rs` (named inotify sinks), shards-apple `fsevents.rs`, shards-ntfs `kept_name`, shards `share.rs`, `daemon.rs`, `daemon/{devices,health,visit}.rs`, `api_run.rs`, init `notice.rs`, `run.rs`, `hotplug.rs`, shards-abi `run/notice.rs`, `NOTICE_ID`, testguest `case.rs`, `watch.rs`, tests/plug.rs |

## Case: reproduced, then fixed (R09, R10)

`tests/plug.rs a_shares_names_are_its_hosts_case_and_all` takes the same steps in a directory
of the host's and in the guest's view of a share of another, and compares each result.

| Run | Host | Revision | Result |
|---|---|---|---|
| Reproduction | macOS APFS, HVF | `a2096f8ef` | red: after `rename Foo Bar`, guest `stat foo` and `stat FOO` ok, host NotFound; and each case-only rename (`Dir`→`dIR`, `low`→`LOW`) done on the host, nothing in the guest ([log](logs/macos-case-red.log)) |
| Control | Linux, KVM | `a2096f8ef` | green: a file system that tells case apart ([log](logs/linux-case-control.log)) |
| Linux's own case-insensitive file system | Linux 7.2.9, tmpfs `-o casefold`, directory `+F` | — | `rename("foo", "Foo")` returns 0, `ls` says `foo`; a directory's alike ([log](logs/linux-casefold-tmpfs.log)) |
| Fixed | macOS APFS, HVF | `6de3842ac` | green ([log](logs/macos-case-green.log)) |
| Mutation: every name trusted its second (`name_valid`) | macOS APFS, HVF | `609a10878` | red, as reproduced: guest `stat foo`, `stat FOO` ok ([log](logs/macos-case-mutation.log)) |

The fix: a name the host found by another case or Unicode form is trusted by the guest for no
time (macOS getattrlist `ATTR_CMN_NAME`, Windows FileNormalizedNameInfo). The case-only
rename is Linux's own (fs/namei.c `vfs_rename`), recorded with its alternatives in D235; the
test holds host and guest to that rule (testguest case.rs `renamed`).

## Notices: a host's edit wakes a guest's watch (R09, R10)

Nothing to reproduce beyond the plan's word and the source: Linux's virtio-fs driver has no
notification queue (fs/fuse/virtio_fs.c, its empty `feature_table`), so before this change no
host edit reached a guest's inotify. `tests/plug.rs a_host_edit_wakes_a_watch_in_the_guest`: a
watcher (testguest `inotify DIR`) in a running microVM, of a bind, a read-only bind and a volume
plugged in as it runs; the host writes a file there, then removes it.

| Run | Host | Revision | Result |
|---|---|---|---|
| Green | Linux, KVM | `6de3842ac` | pass: `IN_MODIFY f` for the write, `IN_MODIFY|IN_ISDIR` of the directory for the removal, each share; the host's files as it left them ([log](logs/linux-notice-green.log)) |
| Green | macOS, HVF | `11698daa1` | pass, 2.18 s for the three ([log](logs/macos-notice-green.log)) |
| Mutation: no request taken for a notice (`noticing`) | Linux, KVM | `609a10878` | red: the removed file's revalidation, an exclusive create no longer refused, made it again on the host (`/rw: [.../rw/f]`) ([log](logs/linux-notice-mutation.log)) |

Its latency is ROOT's to measure (R14): [run.sh](run.sh), which runs tests/plug.rs
`notices_latency` (`--profile release`, n, p50, p90, p99, max and each sample in ms, host edit
to the guest watcher's line reaching the host, its report back included).

## Light tests

`SHARDS_HELPERS=skip`, empty placeholders for the guest binaries `build.rs` embeds.

| OS, arch | Revision | Result |
|---|---|---|
| Linux x86_64 (adacachy) | `bad423565`, `6de3842ac` | clippy `-D warnings` of shards-init, shards-testguest, shards-abi for x86_64 musl, and of shards-vmm, shards-abi, shards: clean. Unit: server's `a_notice_changes_nothing_on_the_host`, `a_name_found_by_another_case_is_trusted_for_no_time`, `a_directory_is_held_while_the_guest_knows_it`, `the_hosts_changes_where_the_guest_looks_are_noticed`; notices' `changes_are_told_once_and_within_bounds`; abi's records; init's `notices_are_told_whole_across_reads`; share.rs's 3: pass |
| macOS arm64 (Mac Studio) | `bad423565` | clippy `-D warnings` of shards-vmm, shards-apple, shards-abi, shards: clean. Unit: the server's three, notices' two (`a_change_is_placed_within_its_root` too), shards-apple's `a_change_beneath_is_told_by_its_path`, share.rs's 3: pass |
| Windows x86_64 (the Windows 11 guest) | `7a8d99009` | abi's records; server's `a_name_found_by_another_case_is_trusted_for_no_time` (NTFS finds `foo` as `Foo`), `a_notice_changes_nothing_on_the_host`, `a_directory_is_held_while_the_guest_knows_it`, `a_guests_directory_tells_names_apart_by_case`; notices' bounds; share.rs's 2: pass. `the_hosts_changes_are_told_where_the_guest_looks`: failed in its own setup (cmd's quoting of the write's path), fixed after |

## R01–R16

| Rule | This change |
|---|---|
| R01 | No unwrap, expect, indexing or unchecked arithmetic in production code: records decoded with `splitn` and `get`; FSEvents' and inotify's buffers read with `get` and checked offsets; every system call's failure returned or the notice dropped. |
| R02 | D235: Lima's `mountInotify` (source `HandleInotify`, `os.Chtimes`), Docker's osxfs (its docs), a guest driver notification queue (no stock kernel), polling; for case-only renames 9p's per-lookup inodes and refused lookups; each with its source. |
| R03 | `MOST` 16384, Linux's default `max_queued_events`; `READ` 64 KiB, a pipe's capacity (pipe(7)); `RECORD_MOST` a tag's (virtio 1.3 §5.11.4) and `PATH_MAX`'s; Windows' 64 KiB, ReadDirectoryChangesW's documented network most; FSEvents' latency 0 (no wait added); `NOTICE_ID` past every reserved ID. Test patience (60 s) is no runtime policy. |
| R04 | A notice is typed by its caller (`noticing`), answered by one match; records decoded as `Decoded::{Record, Partial, Malformed}`. |
| R05 | No new cache: names' validity asked each LOOKUP; a directory held is the node table's; Linux's watches follow the guest's nodes, one instance; no daemon thread or poll reads notices: a pipe straight to the guest's built-in. Its stream held with the run. |
| R06, R07, R13 | Focused functions (`name_valid`, `stored_as`, `kept_name`, `watch_dir`, `holds_dir`, `Pending::tell`, `Mounts::touch`); comments for obligations and sources. |
| R08, R14 | No performance claim; the per-LOOKUP cost on macOS and Windows and the notices' latency are ROOT's to measure. |
| R09 | Case reproduced live (above) before the fix; notices from the driver's source and the plan. |
| R10 | Red, green and a mutation for each; logs in [logs](logs). |
| R11 | Light tests on Linux, macOS and Windows; real VMs on KVM x86_64 and HVF; unverified: WHP (no hypervisor in the Windows guest; jupiter's runs ROOT's), arm64 KVM (L1, ROOT's). |
| R12 | One server, one pending set, one record format; each host's watcher its own mechanism, the smallest adapter (inotify, FSEvents, ReadDirectoryChangesW). |
| R15 | Implementer; merged to main fast-forward under the person's 2026-11-12 rule. |
| R16 | Work in `~/g121-tmp/s8p2*` on the Mac Studio, `~/g121-tmp/s8-src`, `s8-target` and `C:\g121\s8` on adacachy and its Windows guest; nothing built on this Mac. |

## Owed to ROOT

- WHP runs of `a_host_edit_wakes_a_watch_in_the_guest` and `a_shares_names_are_its_hosts_case_and_all` (Windows x86_64), and arm64 KVM (L1).
- The notices' latency (`run.sh`) on each host; the LOOKUP cost of asking a name's case on macOS and Windows.
- Notices for join shares (D119), direct boots' shares and clients' relayed directories, none built.

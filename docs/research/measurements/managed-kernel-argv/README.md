# Required x86 kernel options and Linux init argv

Status: scoped implementation; coordinator real-guest green/mutation gates pending.
Baseline combined `cab396cecf6e94e307c7dac3480844112ecd245e`. Policy read:
`c921a0b12b06d495b354907162e991376edffd41`, rules SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Coordinator owns real VMs, mutations, profiling, audits and combined gates;
constants lane is implementer, scoped code and light checks only.

## Reproduction and decision KARGV-1

Coordinator r168 boots the experimental Linux kernel but the native probe rejects
its arguments. Coordinator r173 at the baseline records actual init argv:
`["/init", "managed-network", "capabilities", "reboot=k", "pci=off"]`.
The capability test fails (0 passed, 1 failed, cargo exit101), after the real guest
boots and powers down. `red-r173.log` is the readonly coordinator log copied from
`~/managed-network-guest-cab3-argv-v1.log` on Fedora
`8fadc1ec213332e16cb0d915a3fb913f`; SHA256
`4239ab8d8e2953cebdebc349002cac9f3d28c0b3abe5f7abeca6bbfbc409252c`.
Its guest contains the coordinator's argv observation instrumentation; the log
retains actual guest/kernel/config identities. This implementer did not rerun it,
change the kernel pin, or change the native networking engine.

The x86 builder appended required `reboot=k`, conditional `pci=off` and WHP's
`cpuhp.parallel=0` after the complete user command line. When that string contains
a Linux init delimiter those words become init arguments. Insert the same ordered
options immediately before the first kernel-recognized delimiter. Keep all raw
bytes from that delimiter through end-of-string, including its original quoting,
whitespace, later delimiters and init arguments. Keep prefix bytes and existing
required-option precedence. With no delimiter, preserve the exact old append
order and spacing, including empty input and trailing whitespace.

### Primary semantics and alternatives

Pinned sources and downloaded-byte hashes are in [sources.json](sources.json):

- Linux v6.18 [kernel/params.c parse_args](https://github.com/torvalds/linux/blob/v6.18/kernel/params.c#L151-L199)
  ends kernel parsing when next_arg returns parameter `--` and no value.
- Linux v6.18 [lib/cmdline.c next_arg](https://github.com/torvalds/linux/blob/v6.18/lib/cmdline.c#L210-L260)
  groups with double quotes, removes a leading quote and an optional final quote,
  accepts unbalanced quotes, and does not implement shell escaping/single quotes.
  A parameter with an equals value is not a delimiter. The private scanner only
  locates this token; exact equality also excludes every equals-bearing token.
- Linux v6.18 [lib/ctype.c](https://github.com/torvalds/linux/blob/v6.18/lib/ctype.c)
  marks HT/LF/VT/FF/CR, space and byte0xa0 with `_S`. Match that byte class, rather
  than Unicode or Rust ASCII-whitespace rules. UTF-8 is retained without rewriting;
  delimiter offsets always start at an ASCII dash or double quote and are checked
  before taking string slices. Unit cases distinguish Unicode whitespace and the
  kernel's classification of UTF-8 continuation byte0xa0.
- Linux v6.18 [arch/x86/include/asm/setup.h](https://github.com/torvalds/linux/blob/v6.18/arch/x86/include/asm/setup.h#L6)
  fixes COMMAND_LINE_SIZE at2048. Reuse x86 layout::CMDLINE_MAX and
  boot::write_boot_state's strict less-than bound/NUL refusal; this is a protocol
  bound, not a learned resource cap. Existing machine options and their conditional
  behavior keep their prior D153/PM M353 justification.
- Established [rust-vmm linux-loader v0.13.2 Cmdline](https://github.com/rust-vmm/linux-loader/blob/v0.13.2/src/cmdline/mod.rs)
  separates boot/init strings, checked capacity and their insertion APIs. Its raw
  constructor searches for a space-surrounded delimiter and trims, while its owned
  strings/CString reconstruct the input. Adopting that broader representation here
  would add allocations and alter accepted raw bytes. The narrow borrowed scanner
  and stack writer serve the one existing x86 assembly call instead.

Rejected: prepending all machine options changes last-value precedence relative
to user kernel options; appending after the entire string retains the reproduced
bug; changing the fixture/environment would hide it; shell tokenization or an
exact `" -- "` search disagrees with the pinned kernel parser. No cache, new manager,
framework or dependency is introduced. Quiet/boot_into append sites remain
unverified follow-ups outside this assignment; this fix does not claim them fixed.

### Ownership, states and bounds

The caller owns one `[u8; layout::CMDLINE_MAX]` stack buffer for this composition.
The helper borrows input/options and returns a checked UTF-8 slice tied to that
buffer. Quote state is explicit Outside/Inside; whitespace ends tokens only in
Outside, double quotes transition state, and the first matching delimiter ends
scanning. Leading-quoted final tokens need no balanced close, matching Linux.
The capacity check precedes parsing/copying, so scanning/copying is bounded by the
existing protocol maximum. Input prefix/tail and each option are copied whole.
Checked length arithmetic and destination/string slices return the existing typed
BootError::DoesNotFit, including on NUL, with its existing command-line address
and total composed byte length. The existing build boundary renders that error as
String. No unsafe code, unchecked indexing or production assertions are added.
copy_from_slice receives equal lengths from a checked range of part.len() bytes.
No heap allocator call is added by composition; allocation counts, stack/fault
costs and performance have not been profiled and no allocation-free/4–5x claim
is made. The earlier kernel_cmdline Cow/quiet path is unchanged.

## Light validation and coordinator handoff

[run-light.sh](run-light.sh) runs only four focused units per Linux libc, strict
lib/tests clippy for GNU/musl, and a compile check for the installed Windows GNU
x86 target. It records the exact compiler commands, exits, source hashes,
compiler/kernel identity and linked unit binary hashes. The Windows GNU check is
additional cfg(WHP) compile evidence, not a passing supported MSVC matrix target.
Each private run cleans shards-vmm artifacts and touches owned inputs explicitly
because kiln source synchronization can preserve stale mtimes. No timing counters
or instrumented parser are used. Unit vectors cover quoting, unbalanced/empty
quotes, equals values, non-shell escaping, Unicode, no delimiter, optional machine
options, exact init tail and max-fit/overflow/NUL behavior.

Run from this checkout on an admitted Linux amd64 runner with unchanged envelope
4CPU/8GiB/20GiB, in a private clone/target:

```sh
CARGO_TARGET_DIR="$HOME/managed-kernel-argv-target" \
RECORDS="$HOME/managed-kernel-argv-light-v2" \
bash docs/research/measurements/managed-kernel-argv/run-light.sh
```

Initial r69 light driver exited1 with no compiler output. A setup defect was
identified: its identity block ended with df on target/.. before creating target.
The corrected driver creates that path first. The failed identity remains at
Fedora `~/managed-kernel-argv-light-v1/identity.txt`; it has not been downloaded
because later exact-envelope admissions were held. Subsequent exact
resource asks were held when fewer than4 CPUs were idle; no smaller ask or guard
bypass was used. No successful Linux compiler invocation occurred before this
checkpoint; GNU/musl/MSVC compilation and strict clippy remain owed. The available
Windows GNU check also remains unrun and is supplemental only. Coordinator
requested the runnable checkpoint while Linux was saturated; no further admission
loop was started. Canonical Windows x86 compilation still requires, in an
authorized MSVC-capable environment:

```sh
cargo clippy -p shards-vmm --lib --tests --target x86_64-pc-windows-msvc -j4 -- -D warnings
```

Coordinator owed: compose/review; run restored capability+12 native guest boots
using the same experimental kernel/config and fixture inputs; independently
restore the old post-delimiter append and observe the actual extra init args/test
failure, then restore the fix and rerun. Use existing
`docs/research/measurements/managed-network/guest/run.sh` with absolute experimental
SHARDS_KERNEL/config and evidence paths; do not change production kernel pin or
use an argv workaround. Root owns the exact real-VM command/input record and
resource/cleanup counts. Full seven-target gates, actual WHP/HVF hardware, other
kernel versions, resource/performance and guest quoting cases remain unrun here.

A distinct coordinator-approved native Mac helper run passed4/4 units under
`lockf /tmp/g121-mac.lock`, pinned rustc1.98.1, macOS arm64; actual free bytes
`41041145856` at its floor check (see actual sources record for exact value).
[native-v1](native-v1/) retains wrapper, commands, compile exit0, unit exit0,
source/binary SHA256 and actual host/toolchain identities. The wrapper includes
the same unmodified cmdline.rs via path. It extracts CMDLINE/CMDLINE_MAX and the
DoesNotFit variant declaration from actual source into a test-only adapter;
**it does not compile the real BootError, full VMM, Linux GNU/musl, or Windows
ABI** and substitutes for none of those obligations. The private executable is
preserved at `/Users/adalundhe/g121-managed-kernel-argv-native-v1/cmdline-unit`.
Reproduce this tiny native check on an arm64 Mac with the pinned toolchain:

```sh
OUTPUT="$HOME/g121-managed-kernel-argv-native-v1" \
RECORDS="$HOME/g121-managed-kernel-argv-native-v1-records" \
lockf /tmp/g121-mac.lock bash docs/research/measurements/managed-kernel-argv/run-native-helper.sh
```

Local rustfmt checks of the owned helper/x86 source, bash syntax checks,
`git diff --check` and `scripts/check-development-policy` pass. Policy script
success does not replace coordinator evidence review. No other agent's source or
cache was changed, no source/evidence was deleted, and no agent job remains active.

## R01–R16 record

| Rule | Scoped evidence and remaining gate |
| --- | --- |
| R01 | Checked stack composition, checked math/slices, typed existing failure; no unsafe/index/assert/panic/heap call added. Light compile/lint recorded separately; allocator/fault profiling owed to coordinator. |
| R02 | Live coordinator red, pinned kernel parser/ctype/protocol and linux-loader alternative above; fixed seam only, broader networking unchanged. |
| R03 | Reuse2048 protocol bound; quote/whitespace bytes from Linux; space separator and NUL semantics are grammar/protocol, no replacement runtime cap. |
| R04 | Explicit quote transitions and delimiter terminal result; typed BootError preserved through existing outer String boundary. |
| R05 | Existing x86 command-line assembly only; no waits/cache in affected path, no new cache/manager/dependency. |
| R06 | Single bounded scanner plus checked writer; optional machine options retain their order and conditions; no generic public abstraction. |
| R07 | Comments cover kernel grammar, byte class, capacity and raw preservation; source links above. |
| R08 | No comparative/performance claim;4–5x target not measured or achieved here. |
| R09 | Exact real guest argv red supplied and retained by coordinator before code change. |
| R10 | Red, source hashes, driver and actual light logs retained; real fixed green/old-append mutation explicitly owed. |
| R11 | GNU/musl/MSVC checks held/unrun; native Mac pure-helper4/4 recorded separately; Windows GNU is supplemental and unrun; seven supported targets and actual VM hosts not claimed passed. macOS arm64 only. |
| R12 | One x86 assembly shared by KVM/WHP; private parser no architecture/OS duplication, extra installed binary, host root or crypto change. |
| R13 | Underlying insertion fixed, raw tail and no-delimiter behavior preserved; quiet/boot_into follow-ups held rather than speculative scope expansion. |
| R14 | No new allocator calls in source, bounded stack storage; no allocation/CPU/fault measurement or claimed performance result. Root owns profiling. |
| R15 | Implementer scoped source/light checks; root owns audits, VM/mutation/perf and final combined gates/merge. Signed named-path checkpoint, no nested agents/main merge. |
| R16 | Mac actual free above30GiB; only the authorized tiny native-wrapper compiler ran under lockf, no full crate/C build or deletion; remote private clone/target/records, exact4CPU8GiB20GiB guard retained. Root clone/target readonly/offlimits. |

# D234's literals (R03): an independent review, and what it changed

A read-only reviewer was given D234, the policy and the sources, and not the author's audit. It
built its own list of the literals D234's grant introduces, then checked each derivation against
its source. It reviewed revision 20f37abee (rules sha256 df2acf47…, read in full).

Below is its report as given, after which comes the response, applied to D234 and to
literal-audit.py in the commit that adds this file.

## The reviewer's report (2026-11-12), condensed

Condensed for this record: the D234 line-number column (at 20f37abee) is left out, as is
part of each row's evidence. The verdicts, omissions and errors are as the reviewer gave
them.

**Verdicts**

| Literal | D234's derivation | Verdict | Checked |
|---|---|---|---|
| `NESTING` directive | "are shards' own" | weak: no naming rule. f31 only shows that no existing directive grants anything to the command | f31's list (AGENT…VOLUME) |
| `--security-opt nesting=true` | "The key is shards' own: dockerd refuses keys it does not know" | weak: it says why the key isn't Docker's, not why `nesting` or `=true`. The obvious sources (Incus `security.nesting`, f32; Docker's `writable-cgroups=true`) aren't named as the source | — |
| shards-api image-spec field `nesting` | none | missing from the names list | — |
| setup word `nesting` | "shards' own" | weak (no rule); no collision | about 40 existing setup words in init and daemon `setup.rs`; none matches `nesting`, `ready-by=` or `gpus=` |
| `ready-by=` and its value: ns since the Unix epoch, host clock | word: "shards' own" | word weak; unit and epoch hold (they match HOST_TIME, so init's conversion is unit-consistent) | abi `lib.rs:102-105` |
| `gpus=` and its `,` separator | "shards' own"; init "checks with `shards_abi::gpu`'s grammar" | word weak: it reuses Docker's `--gpus` word (a render node in shards) for `--gpu-device` identities. Separator holds | abi `gpu.rs:79-125`: the grammar has no comma |
| leaf `init` | "the name moby's hack/dind gives the leaf a nested runtime moves itself into (f35)" | holds | hack/dind@1e9198df:63-66, 67-78; init `run.rs:1171` |
| controllers cpu, cpuset, io, memory, pids | "those init already enables for `run`" | holds | init `run.rs:1219` |
| CAP_SYS_ADMIN 21, CAP_NET_ADMIN 12 | "the kernel's numbers" | holds | Linux v7.2-rc4 `capability.h:205`, `:281` |
| `pivot_root` added | f16 | holds | vendored `default.json`: no rule names it |
| `bpf`, `perf_event_open` | "as Docker's profile opens them for CAP_SYS_ADMIN" | holds | `default.json` rule 17 |
| socket families 38 and 40 | "its `socket` rules skip families 38 and 40" | holds | rules 2-4; `socket.h:244` AF_ALG 38, `:246` AF_VSOCK 40 |
| io_uring, userfaultfd, keyctl, kexec | "no rule names them" | holds | none of the seven syscall names appears in any rule |
| modules | "only CAP_SYS_MODULE opens them" | holds | rule 23 |
| `fs.nr_open` target | "the run's own ask" | holds (caller's data) | — |
| `NR_OPEN_MAX` = 2147483584 | "computed in init from fs/file.c's own formula … never chosen" | holds | `fs/file.c:100-101`; init `setup.rs:667` |
| `SHARDS_READY_BY` (the name) | "takes the place of Hex's HEX_BOOT_READY_BY" | holds as a naming rule | `SHARDS_DOMAIN` precedent, init `domains.rs:494` |
| CLOCK_BOOTTIME, ns, decimal text for `SHARDS_READY_BY` | "the clock Hex's HEX_BOOT_READY_BY uses (f24)" | weak: f24's two quoted Hex lines name neither CLOCK_BOOTTIME nor a unit. Unit and format are underived | `time.h:56`. Hex not checked |
| `/run/shards/notify`, `/run/shards/gpus` | "beside `/run/shards/engine.sock` (D181)" | directory holds; the leaf names `notify` and `gpus` have no rule | abi `run.rs:30`; D181 |
| identities file mode 0444 | "readable by every user of the run …" | holds | init `setup.rs:655`, `:660` |
| one identity a line | "one a line in `shards_abi::gpu`'s grammar" | holds | longest identity 22 bytes |
| GPU count at most `MAX_PHYSICAL_GPUS` | "at most D232's `MAX_PHYSICAL_GPUS`" | weak: D232's own Literals say it is "owed a derivation of shards' own (R03)". Its wire cost is not addressed | abi `gpu.rs:17` |
| `NOTIFY_SOCKET`, `READY=1`, datagram socket | "sd_notify's" | holds | systemd@b4259f37 `sd_notify.xml:181`, `:582-584` |
| notify socket mode 0666 | "as engine.sock's …" | holds | `engine.rs:52`; Linux `af_unix.c:1217`; `readiness.rs:152` |
| `WORKLOAD_READY` = 7 | "next free value … 1 to 6 on main at 43114652a" | holds | abi `lib.rs:148-161`; no branch of 1,022 takes marker 7 or above |
| `run::kind` 49 (READY) | "(0 to 3 and 16 to 48 are taken)" | holds as "after the highest" | abi `run.rs:55-184` |
| ipc kind 90 | "(kinds run to 89; 128 and 148 stand apart)" | the value holds; the parenthetical is wrong; the kind's name is not given | ipc `lib.rs:186-559` |
| READY payload: 9 bytes, state 1/2/3, big-endian u64 ns | "in the order of the table's reportable moves …" | holds as identifiers; the order rule has a mismatch | `run/nesting.rs:132-134` |
| init's `notify` entry and tag `N` | "as engine's listener is `E`" | holds; no clash | `run.rs:2337-2339`; chooser's `N` is on its own socket |
| PIPE_BUF datagram bound | "systemd's NOTIFY_BUFFER_MAX (constants.h:35, f36)" | holds as parity with systemd's receiver | `constants.h:35`; `notify-recv.c:97`, `:129-131` |
| sender-cgroup rule; unknown keys ignored | the two quotes | both quotes verified | `systemd.service.xml:1236-1237`; `sd_notify.xml:531-532` |
| on-time boundary | stated rule | holds | `readiness.rs:207` |
| the deadline value | "the caller's, and none where it gives none" | value and uncertainty hold; how the caller gives it is missing | — |
| run event for the relay | none | missing | — |

**Literals D234 introduces that its Literals section leaves out:**
1. How the caller gives the deadline.
2. Cold start: a deadline before the guest's boot becomes `SHARDS_READY_BY=0` (`readiness.rs:38`).
3. `/run/shards`'s mode 0755 (`readiness.rs:166`, `engine.rs:40`).
4. The credentials control buffer `[0u64; 4]` (`readiness.rs:253`). Its derivation (CMSG_SPACE of a ucred, 32 bytes) is correct but lives only in a code comment.
5. The names of ipc kind 90 and of the run event.
6. The shards-api field `nesting`, and the leaf names `notify` and `gpus`.
7. `gpus=` against `run::MAX_PAYLOAD`: a maximal entry is 5 + 65,536 × 22 + 65,535 = 1,507,332 bytes, inside the spec's setup list, which shares MAX_PAYLOAD (6,291,456 bytes) with argv and env.

**Wrong or unsupported derivations:**
1. "128 and 148 stand apart" is wrong. Neither is an ipc kind: 128 is `RUN_SWAP`, a RUN flag bit, and 148 is `SAVED_STATUS`, an exit status. Also, ipc already has `kind::READY = 1` with another meaning, so naming the relay READY would collide.
2. READY's state codes "in the order of the table's reportable moves": the table lists "ready-by passed" before "late", but the codes are late = 2, passed = 3. Why 0 is unused isn't said.
3. CLOCK_BOOTTIME and nanoseconds rest on f24, whose quoted evidence supports neither.
4. `MAX_PHYSICAL_GPUS` is a bound that its own source, D232, says is still owed an R03 derivation.
5. "Next free value" is ambiguous for run kinds, since 4–15 are also free. The stale branch `p/managed-credentials-input-kiln-v1` defines run kind `INPUT_BYTES = 49`; it already collides with main's 48, so it must renumber anyway.

Not a D234 literal, but relevant: systemd pairs NOTIFY_BUFFER_MAX with a datagram queue length of 512 over the kernel's default of 10. D234 says nothing about queue length.

**Not checked:** Hex's source (no checkout was named), and nothing live.

## Response, applied to D234 and literal-audit.py

**Names now carry their rules:**
- `NESTING` is a noun naming what it declares, as AGENT and NETWORK are, and Incus's word (f31, f32).
- `nesting=true` is a key=value pair, as `writable-cgroups=true` is (D42), keyed by the grant's name.
- shards-api's `nesting` is the same name, and its `ready_by` is in snake case.
- The setup words name what they carry. `notify` and `gpus` are named for what each file holds.
- `SHARDS_READY_BY` follows `SHARDS_DOMAIN`'s precedent.
- The run's events `ready` and `ready-by-passed` are the states' names.

**The clock:** CLOCK_BOOTTIME, decimal nanoseconds and the digits-only form are now cited from Hex's own lines. f24 gains guest.rs:233 ("the instant on the guest's own clock, `CLOCK_BOOTTIME` nanoseconds"), :258 and :261, read at Hex main b1891329.

**The deadline's input and its cold start:** the caller gives shards-api's `ready_by`, a budget, which D231's run host converts to the host's wall-clock instant. A run from the CLI gives none. A deadline already past becomes `SHARDS_READY_BY=0`, and ready-by passed at once.

**ipc kind 90 is named `WORKLOAD_READY`,** because ipc's kind 1 is already READY. The wrong "128 and 148" parenthetical is replaced: those are a RUN flag bit and an exit status. "Next" now means the next above the highest, with run kinds 4–15 left as the table left them. The stale branch's collision is recorded.

**READY's state codes** are numbered by how far past the deadline the moment is: on time 1, late 2, passed 3. 0 stays unused, so a zeroed payload names no state.

**New literal bullets in D234:**
- `/run/shards`'s 0755;
- the 32-byte credentials buffer;
- the notify queue: the kernel's, not raised, since one datagram suffices;
- the cost of `gpus=` against `run::MAX_PAYLOAD`: refused past the bound, never cut. D232's own owed derivation of `MAX_PHYSICAL_GPUS` is named as D232's.

**literal-audit.py** plans 31 literals. Each identifier's own constant is matched exactly: `^WORKLOAD_READY$` for marker 7 and ipc kind 90, `^READY$` for run kind 49.

# Additional existing-cap correctness checks

This is the first GNU checkpoint; [MUSL-0E.md](MUSL-0E.md) records the later
landed-source musl validation. Production stayed frozen at
`72c202955bb5604aef495a91619c6bf885f6f1a0` while ROOT handled its combined musl
correction and integration. These six tests live only
in `cap-correctness.rs`, under this evidence directory. The private light package
appends a `cfg(test)` child declaration to the exact original admission module
body, then imports that module with the real libc/testdir and workspace lints.
No production source, registration or public API is changed.

Actual admitted Fedora GNU run: build0, **15 passed / 0 failed / 0 ignored /
0 filtered**, Clippy `-D warnings`0. That includes the original nine cases and
all six new cases; the driver requires the exact count, rather than accepting
a successful empty filter. The raw unit log is in `records/caps-v5/units.log`.

| New case | Actual ordinary-file witness | Remaining kernel/caller boundary |
| --- | --- | --- |
| Missing controller | Typed ControllerUnavailable; subtree sentinel unchanged. Already-enabled controllers leave their file unchanged. | Real delegation and kernel enablement remain ROOT-owned. |
| Missing kill or events | Each missing lifetime control prevents Committed, retains ENOENT and failed-rmdir ENOTEMPTY from the private files. | No actor was launched; real pre-release refusal/cleanup still needs a guest witness. |
| Explicit zero tasks | Zero readback stays Value(0); verifying it as Max refuses. | Does not prove a kernel clone denial or protected helper/control capacity. |
| Oversized/overflow scalar | Excess input is InvalidNumber, decimal overflow is Overflow, numeric u64::MAX stays distinct from Max. | Does not establish the kernel's accepted numeric range. |
| Parent path replacement | A held original parent receives the new directory after its pathname is replaced; unused removal acts under that original parent. | No claim about hostile concurrent cgroup mutation or cross-process lifecycle races. |
| Adopted existing parent | Refusal reports NotCreated and preserves the existing private directory. | Does not establish kernel cgroup population/lifetime cleanup. |

The page value4096 in these regular-file setup cases is only a fixture input;
none requests a memory value or emulates kernel page rounding. Production still
uses sysconf. No cap becomes a guaranteed reservation, and no case admits a new
owner claim or alters D59 victim policy.

Exact identities:

- Original production body:26438 bytes, SHA256
  `6d478025241a340b2eb4bcf4ef420c192ea64c5089c512b911ffdcd74fc2421f`.
- Private compiled module, including only the child declaration suffix:SHA256
  `e94a5c6241d8062ecce4689b7b44cfa62ef6abc9889ac03736496747cfa7c247`.
- Six-case leaf:4731 bytes, SHA256
  `d6c78171bec9b2518cac7f3126a71652af8b36ccf129c9f26c5e12cafb78daf5`.
- Input archive:58115 bytes, SHA256
  `6e324fe2f947d7c5fac1ed5328ba1d7441b94e2a7655ba6bd7c281243700988a`.
- Actual executable:7483960 bytes, SHA256
  `e015b2d9d9c7b5d441c0eb2ba30bf11f08de496177792121afd48d9ae7bcd99c`.
- Original raw archive:6087 bytes /19 regular members, SHA256
  `1f7e8c2f43c7c9263dbaf4abb8aac3c95663d506972e375787b81d2803c4f7ae`.

Runner `8fadc1ec213332e16cb0d915a3fb913f`; new private
`/home/adalundhe/managed-resource-admission-{source,target,light}-72c202955-caps-v5`.
Admission was serial `sync:false`,1 CPU/1 GiB RAM/2 GiB disk,180 seconds per
command. No local compilation, root cache write or runtime VM/controller launch.
All own jobs are terminal; sources, Cargo caches, executable and raw records
remain preserved. A terminal job and regular-file cleanup assertions are not
process-population, kernel-accounting or leak-freedom evidence.

Exact build/test/Clippy commands, observed Rust/Cargo tool versions, environment,
source derivation and binary hash are retained in `records/caps-v5/`; the exact
outer invocation/result and source archive are adjacent. The19 original members
and archive were byte/hash verified on collection. The libtest final blank line
is retained and classified, not stripped or broadly exempted.

The README's full R01–R16 account applies: these are targeted implementation
checks of a new cap-admission requirement, not a reproduced kernel defect fix,
audit, profile, benchmark or performance claim. ROOT still owes exact combined
musl/seven-target and carried-helper compilation, actual cgroup page-normalized
readback/refusal/rollback/control survival, identity/FD/population cleanup,
meaningful mutations/restoration, and parent/kernel attribution. Hierarchical
guarantees, protected control capacity and the missing checked owner-claim
producer remain open.

# Pure native-record width correction

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Rules SHA-256 remains
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
This changes only the approved test runner's native-record acceptance predicate
and synthetic unit coverage. No production allocator, native query, platform
seam, fixture, global registration or tenant source changed.

## Reproduced input-contract failure

Signed `dd75ffc837605613dd20cf95e46233f4b5af3ab4` adds two pure tests: valid
conditional records for all three roles, including a zero delta, and invalid
source/width/identity/bool/negative/out-of-width cases. The otherwise matching
synthetic VM record with `ConditionalDifference`, Linux aggregate label,
`native_bits:64` and `value:18446744073709551616` was incorrectly accepted as
`Conditional`. That value is 2^64 and cannot fit the existing Rust producer's
`Delta::ConditionalDifference(u64)` field. This is not an observed OS counter,
fault rate, native saturation, overflow campaign or production defect.

[Original red records](records/paired-dd75ffc83):13 unit methods,12 passed,
one failed at `test_native_invalid_source_width_identity_and_delta_are_unknown`.
Its failing subcase is exactly the out-of-width value; the valid conditional
and zero cases and the other negative cases pass. Source was unchanged paired
runner SHA-256 `32473517d77a9df4a5ed4d192baf6c01a2aa5bf30ca7474a3d356a37cd1dc016`.

## Narrow correction and validation

Signed `662d564efc1001b1647fe530e2ae4ac630da308b` adds the producer-derived
`value < (1 << 64)` condition to the existing acceptance predicate. Nonnegative
integer validation, required Linux source/width/role identity and conditional
status remain; invalid coverage stays `Unknown` through the existing path.
The unsigned representation bound is not a measured fault limit, allowance,
chosen threshold or protection against native wrap/reset/parser fallback.

[Green records](records/paired-662d564ef):the same13 tests pass with exit0.
Current paired source SHA-256 is
`1e139e610572f834398baae4138f1a1154ccaa64b9994fab410702010a22a173`.
Both runs use test source SHA-256
`0e4af89edd37d2757e100f506f1207672c114ee748492e875d87abb247b4df44`.
Each raw directory retains exact source, invocation, stdout/stderr, exit,
Python identity and six verified original record files. No original is normalized.

Both invocations used Kiln Fedora `8fadc1ec213332e16cb0d915a3fb913f`, admission
`linux/amd64,cpus=1,mem=1g,disk=2g`, `sync:false`, timeout40s. Each uses NEW
private `$HOME/managed-allocation-metrics-{source,target,light}-paired-<full-revision>`
paths. Full executed commands are in each `kiln-invocation.json`; the only unit
invocation is `PYTHONDONTWRITEBYTECODE=1 SHARDS_METRICS_LIGHT_TMP=<private-target>
python3 -m unittest -v test_paired`. Python was3.14.3, `/usr/bin/python3.14`,
SHA-256 `40850fe1e2df0c14378c12bfaa02b5cda35619e09a8ee50ca3cf2bdb36fc019f`.
There is no new compiled test binary.

The tests import pure runner helpers and manipulate synthetic JSON and ordinary
private files. They never invoke `paired.main`, `run_case`, actual source
preflight, an artifact/fixture, process query, socket, VM, namespace, BPF,
profile, benchmark, stress or mutation campaign. Actual error/timeout/identity/
cleanup behavior of the executor remains a separate root gate. Root still owes
new native caller/helper builds and execution, all actual paired/interleaved
same-N release measurements with caller-supplied n, physical accounting,
instrumentation/noise analysis, adversarial mutations/exact restoration and
cleanup/matrix gates. Prior033c pilots are not passes for this new source.

R01/R04/R06/R07/R13: bounded test-data parsing, explicit Unknown, no changed
control or unsafe path. R02/R03/R05/R12: bound derives from the existing native
model's unsigned field and Linux source label; no new adapter/interface/cache.
R08/R14: no performance or native measurement claim. R09/R10: exact red before
correction and same-input green retained; no real-effect mutation claim.
R11: pure Fedora coverage only here. R15/R16: admitted light scope, signed named
paths, no children/main merge/local build/cache, root targets and paused Windows
ARM preserved. All remote jobs complete; their evidence and targets remain.
The startup147-line proposal remains unapplied and pending root selection;
the sustained lane/parent goal remains active.

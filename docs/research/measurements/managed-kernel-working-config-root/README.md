# Working kernel configuration mode: reproduced correction

The experimental kernel build stopped when Linux's unchanged `merge_config.sh`
tried to overwrite `.config`. The evidence harness freezes its input files
read-only. The recipe's initial `cp` also created a read-only working copy.
The correction adds owner-write permission to **only the new working copy**
after that copy and before the merge. Frozen inputs remain read-only.

ROOT independently reproduced the exact error under ordinary UID1000 on Linux:
`cp: cannot create regular file '.config': Permission denied`. The minimal
case uses two empty files in a writable private directory. Both that case and
the original upstream merge command produced statuses **1, 0, 1** for read-only,
owner-write, and restored read-only. No config redirection was set. The single
changed variable was the working file's owner-write bit.

The exact merge consumed the original 97,971-byte base, shared/x86 fragments,
512-byte experimental network fragment, and unchanged upstream helper. All five
source hashes and modes remained unchanged. The merged output is 102,003 bytes,
SHA-256 `723d968865724d10173caf3dc0212ee344af09239e899dd98df8f6026fdb7979`.
It still needs `olddefconfig`: this is **not an effective kernel configuration**.
No kernel compilation, VM, hook or traffic-interception result is established.

The original failure remains in the interception lane's
`managed-route-enforcement/managed-kernel-native-v4/` record. Its 96 exact
integer-macro comparisons are distinct from the configuration failure and do
not close the ten older bc math-library failures. The correction applies to a
fresh v5 experimental build; no stopped scope or deadline was resumed.

## Repeatable example and retained records

`red.py` and `owner_write_probe.py` are the exact dispatched Python bodies.
On a fresh normally admitted unprivileged Linux account, run `python3 red.py`,
then `python3 owner_write_probe.py`. They create exclusive private directories
under the account's home and refuse to overwrite an existing experiment.
GNU `cp` and the actual upstream helper are exercised; there is no fake merge.
The local dispatch JSONs retain the exact commands and resource envelopes.

`actual-mode-raw-v1.tar.gz` contains all 40 original payloads plus their manifest:
125,894 bytes, SHA-256
`134991aefc33983cb588d45b46f88dff510ef358efaa6c164d6434ecc96d6828`.
This includes original source inputs, both minimal red receipts, full merge
stdout/stderr, before/after file facts and resulting configs. ROOT verified every
member's length/hash and safe unique regular path. `verify.py` repeats that
offline verification and checks all six differential outcomes. Its success is
evidence verification, not another runtime execution.

The first local recovery read mistakenly treated a binary zlib packet as base64;
that read failed before any probe. `packet-read-correction.json` retains it.
The unchanged packet was then decoded correctly. Actual runtime probes had no
failed setup, timeout or retry. No private tokens or credentials were read.

## Policy and limits

Coordinator `01a148884a4770fdaf0111f3f792a248` read full policy at `3830327b6`,
rules SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Each native probe requested 1 CPU, 256 MiB memory, 1 GiB disk and 120 seconds; the original
60-second per-command bound is a test deadline, not production policy. No root,
installed tools, compiler change, old-scope mutation, cache, polling, watcher,
security change or cleanup was introduced. The only permission change was on
new disposable working files owned by the probe.

R01/R04/R06/R07/R13: test code and explicit differential phases only, no
production changes. R02/R03/R05/R12: exact original upstream command plus live
one-variable evidence; no architectural choice, arbitrary runtime constant,
new abstraction or duplicate platform path. R09/R10: retained exact red,
minimized red, green and independent restored-red controls. R08/R14: no cost or
performance claim. R11: GNU/Linux amd64 fixture only; no other platform result.
R15/R16: coordinator-owned bounded probes, intact original sources/evidence and
no bypass of headroom or existing held campaigns. All broader management,
effective-config, kernel-build, route, hardware and cost gates remain open.

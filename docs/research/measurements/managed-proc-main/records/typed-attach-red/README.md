# Precise attach-after-stop assertions: additional live result

This record preserves two coordinator-observed fixture failures and a subsequent
focused result. Production typed observations are not changed by these tests.
Policy f5/d634 and the [typed batch account](../../TYPED.md) remain applicable;
implementer scope is two client assertions/light units, no production or broad
audit/campaign. Root owns the full corpus, matrix and final readiness review.

Root full ordinary corpus at main compositionbf2535/r345 reported
lib40passed/2failed/5ignored and sim15passed/0failed/2ignored. The two existing
client attach tests failed after their owned stop/wait with exact matching
Process::Exited, while their assertions allowed only Os at pidfd-open or exit
registration with ESRCH. The [read-only root log](root-r345-lib.log), SHA256
`79305410ecbeb8ba4f3e30898e3c0665be7999203cb61fbf2978f5d1d057312d`, is retained;
[provenance](root-r345-provenance.json) records its original root-owned location.
No root clone/target was changed.

Root approved only adding exact Exited under the same PID/expected-start guard to
these two assertions. Signed source
`82a6b7fdaaf18aca288dda10f743f845453fcd7d` changes exactly those two lines in the
client cfg(test) module. Existing exact native operation/raw-ESRCH predicates
remain. Production bytes are unchanged from73c85d039. Committed launcher
`3f718773c5ff3fdea5f00842ab1eaac27bdb0224` runs only the two ordinary tests:

```sh
bash docs/research/measurements/managed-proc-main/run-typed-attach.sh
```

Native Fedora GNU amd64 r231 requested exactly4CPU/1GiB/2GiB once, admitted with
21/32 idle. It actually rebuilt the selected executable (`fresh:false`) and ran
two cases at exact3f718. Result: **1passed/1failed**, cargo101, wrapper1.
`attach_takes_back_a_guest_whose_client_crashed` passed.
`attach_takes_back_a_guest_whose_handle_was_lost` failed at client.rs1523 with:

```text
Process { pid: 1143905, expected_started: 10413725,
          failure: Os { operation: ReadStat, code: 3 } }
```

Code3 is this native Linux ESRCH value. It occurred at ReadStat after opening the
stat file, rather than at the existing permitted hold/register stages. This is
not a noise classification or permission to accept generic Process/OS failures.
The source/actual failure was sent to root before any extra predicate change.
No ReadStat alternative has been applied at this checkpoint; root's further
precise-boundary review and full rerun remain owed. Conformance was left unchanged
because this focused run did not execute it.

Selected executable SHA256
`33b347c49d7855737be7f8108852c737ef1e6d0a06f29161a17bcf5c79cf4579`,
immutable copy `~/managed-proc-typed-evidence/attach-v1/test.bin`.
Original [raw archive](original-raw.tgz) SHA256
`e482d394358838fbf96f36f113010653ffd0af87b3e71d1d41dbc4970372a7f0`,3091B.
[Raw extraction](raw/) includes exact command/Cargo JSON, source status/hashes,
toolchain, selected binary metadata and actual exit. Selected source hashes match
the committed Git blobs. No full corpus, new libc/platform run, mutation,
threshold/cache change or production fix was performed. Old6db/r224/r227/r229
evidence and all earlier immutable executables remain preserved.

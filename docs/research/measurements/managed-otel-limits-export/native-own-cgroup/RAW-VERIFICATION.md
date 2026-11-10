# Original native archive retrieved and verified

This is an evidence follow-up to signed81632cc92fcaa7b11d3844e0309dcc94fa397345.
The native fixture, production Reader/codec/reference, binaries and results were
not changed or rerun. The initial read refusal and all first compiler failures
remain retained. POLICY_READ implementer current0342/rules SHA256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3;
complete policy/plan/updated handoff reread for the next G25 slice before work.

Normal inventory reported Fedora26/32idle,56GB free. The SAME exact read-only
1CPU/256MiB/1GiB/<=600s/sync:false input then returned0. There was no guard bypass,
envelope/timeout change, different cgroup, native/compiler/test retry or fallback.
[transfer-retry.json](records/light-v2/transfer-retry.json) records the actual
input/admission/output identity; [transfer-denied.json](records/light-v2/transfer-denied.json)
is the original before-start0idle refusal.

The original [raw.tar.gz](records/light-v2/raw.tar.gz) is28609bytes, SHA256
75766b677283916248de29e7aaac209236db41c6ee7ec034082c749fa9bf0e8d.
EVERY unique safe relative regular member matched its original length/SHA256:
**53 original payloads plus one embedded manifest**. [archive.json](records/light-v2/archive.json)
has54 rows including an explicitly labelled manifest row. [result.json](records/light-v2/result.json),
the native payloads/source/error/FD JSON, and selected stdout/stderr are exact
copies of original members. They were not normalized or reconstructed from the
returned metadata. [verified.json](records/light-v2/verified.json) records the
member/semantic/source association checks.

The original resolver stdout proves3passed/0failed/0ignored, with1 filtered
existing official boundary test. That filtered test is not passed by this run.
The native executable actually performs its complete official Rust typed
comparison, which passed for the observed Snapshot. All13 command phases returned0.
Actual installed tools: rustc1.98.1/48a229ceaefd4985c50990b14116b6d856af0985,
LLVM22.1.8; cargo1.98.1/797e8a9bc; rustfmt1.9.0; clippy0.1.98. They do not turn
the repository MSRV into a rust-toolchain pin.

The actual own source is a **non-root** delegated group beneath the fixture run's
service, mapped to the cgroup2 mount at `/sys/fs/cgroup` with mount root `/`.
Both own-membership records match; held mountID43, device29 and recorded inode478878
remain in original source records. Device/mount ID were checked; inode was
recorded, not an atomic inode-to-membership proof. No other PID/group or ancestor
was queried. The underlying reason for absent controller files is unestablished;
no controller policy was changed to obtain a positive result.

Both Snapshot limits and separate fixed-name raw-read results are Io(ENOENT2).
No successful memory.max.raw/pids.max.raw file was fabricated. Other memory/
current-task/IO observations also remain typed Io2; CPU was independently observed.
The actual native request contains1resource/4metric descriptors/14points: six
limit-state indicators, six error-detail points, error-count6 and the CPU point.
All state indicators are explicit0/1 with unavailable=true; both numeric limit
Gauges are absent. Native payload3926B/SHA256
d0c646df8f4e867588a9559e26915dc442f5b1d824906c3cf32a4938fa099ed0;
official compact4879B/SHA256
c8287ef5a7fa96b5fd371283f659e3603df2f1059bee6908dfe01b1fa3c39d8d.
These are original payload identities, not performance results.

FD sets `[0,1,2,3]` match before/after and include the temporary enumeration FD.
TMP files/symlinks are empty; native/test binary before/after hashes match.
Both source archives remain fully verified; current fixture source matches the
actual compiled hashes and c298 Reader/codec/reference remain byte-identical.
No local build, source cleanup, protected ROOT cache access or paused-VM action.

Full staged whitespace check returned2 on ONLY the authentic libtest stdout's
original blank line at EOF (`resolution-tests.stdout:8`). Its bytes remain
unchanged. The separate check excluding ONLY that exact raw file returned0;
whitespace-full.* and whitespace.command.json retain both results. A full-zero
whitespace claim is not made for this evidence addition.

R01–R16 proof limits remain the [original scoped record](README.md#r01r16-and-owed-root-gates).
This follow-up resolves only pending original-archive retrieval/verification
under R10/R15/R16. Live finite/max/error-recovery transitions, real managed caller,
Collector/official Go decoder, VM/seven-target/mutation/reset and all performance/
parent/kernel attribution gates remain ROOT-owned and unverified by this slice.

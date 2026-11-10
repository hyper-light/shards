# Coordinator platform gates

Policy: coordinator c921a0b12b06d495b354907162e991376edffd41,
R01–R16 SHA2561ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d.
Mac arm64 runner b24c5c951260dcdfd0972f2aeaf3cf51, exact Rust1.98.1,
zig0.16.0/cargo-xwin0.23.1. Actual compiler/platform/disk records are retained.

r177 at ac5fe69eb39589b074e4e13e3b7d99ca3ab0a944 passed full `scripts/lint`:
format plus the canonical seven target workspace/all-target checks and the
script's GPU variants. Exit0 is retained with the complete51485-byte log.
This covers the Windows capture handle-sharing fix; it predates the later
kernel-argv, borrowed API and cleanup branches. AWS-LC's ARM SHA3 feature probe
reported an unsupported compiler flag and selected its existing supported
implementation; the build completed. That warning is preserved, not a crypto
configuration change or claim of the omitted optimized implementation's speed.

r179 separately passed the native amd64 Windows pipe test at the same ac5f
source: `pipe::tests::a_shared_plain_handle_survives_the_original_stream_and_keeps_half_close`,
1 passed/0 failed/70 filtered. It verifies the duplicate remains usable after the
original wrapper drops, exact bytes and half-close. Raw identity/log/exit are in
windows-native/ with their original bytes. This is not a full managed VM/WHP gate.

r189 at4d3f454f194a0b6927423f0f859071147c43786d separately passed strict
`shards-vmm --lib --tests` clippy for amd64 Linux GNU, Linux musl and Windows MSVC.
It compiles the corrected x86 kernel-option placement under KVM and WHP cfgs.
The host job used4CPU/8GiB/20GiB and the existing private warm target under lockf.
An initial preflight stopped on Finder-created untracked .DS_Store files before
any checkout/compiler command. The corrected preflight allowed only those exact
metadata names, preserved them and still required clean tracked/indexed source.

Archive SHA25663bda5918421c37439df4b1af10c5d429ee876fa16ea6837222e8e4f03952f0a,
6534 bytes. Extracted regular relative files were hash verified. SHA256SUMS
records raw bytes. Root used the existing private clone/target and did not alter
an implementer's active job. Real Windows/HVF/KVM behavior is a separate gate;
the matrix alone proves none of the new management layer's isolation promises.

R01/R04/R06/R07/R13: no production code in this evidence batch; compile/lint is
one check, not a panic/state proof. R02/R03/R05/R12: existing tools/seams and
canonical target inputs; no architectural choice, tuning literal, cache, new
installed component or security weakening. R08/R14: no performance/allocation
claim. R09/R10: full identities/logs/statuses retained for each exact source,
including setup failure classification above. R11: all seven builds passed at
ac5f, three affected x86 builds at4d3f; newer combinations/real hosts remain
unverified. R15/R16: root validation, admitted resources, lockf, source/cache
ownership and raw evidence preserved; paused Windows ARM VM untouched.

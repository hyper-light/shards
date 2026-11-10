# Private compiler selection after a retained preflight failure

Actual r171 was normally admitted on adacachy with12idle/81GB free and room for
the unchanged2CPU/4GiB/24GiB request. It checked out/froze source976, verified the
source hashes, passed policy, and recorded Rust1.98.1/LLVM22.1.8/Cargo1.98.1.
Host PATH supplied Zig0.17.0. The explicit repository0.16 check stopped the job
with1 BEFORE Rust compilation, pure/Clippy or native tests. This is a real tool
preflight failure, not a reproduced product/actor defect or passing fullcaller.
Original records/build and build-r171-preflight-result.json remain unchanged.

Canonical source976 `scripts/install-zig` pins official Linux x86_64 Zig0.16.0:
`https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz`, SHA256
`70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00`.
The next command selects precisely that compiler within the already owned
runtime-cache/toolchain. It verifies the pin before data-filtered extraction,
uses only private process PATH and retains provenance/version/commands/logs.
No host/global install, persistent PATH, trust/security setting, source change,
fallback to0.17, retry/download loop or cache deletion is introduced.

The original pre-r171 queue receipt seq7550 is timestamp1794349989.118. Adding
the authorized3600s yields conservative deadline1794353589.118, slightly before
the real r171 start plus3600. The private selection/first compile uses ONLY time
remaining to that original deadline, both internally and at Kiln admission.
No build-budget reset or native-case retry/time increase occurs.

Actual r172 was normally admitted on the same GNU runner with4idle/84GB free and
room for the same2CPU/4GiB/24GiB,3199s remaining. It reuses only this NEW still
uncompiled private target. New records/build-pinned-zig-v1 prevents overwriting
any r171 preflight command/output. The unchanged fixture source is976; runtime
case count is still0. Actual compiler verification/build/artifact results are
pending at this record, not presumed from setup starting.

The original approved command texts are retained. Separate pinned-build,
pinned-checks and pinned-native command texts select the private compiler and
new build-record folder, preserving all source, positive assertions, actual
one-Open/no-ACK/FIN/replay/ACK/owner-stop semantics, finite input and bounds.
The eventual single native call still requires same-source fullcaller artifact,
pure7/0/5ignored, strictfullcaller0 and a fresh normal admission. Source976 stays
separate from ROOT's later7fe Capturing integration/E0004 gate.

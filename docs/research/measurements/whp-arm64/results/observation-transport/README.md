# SSH observation failure: native task state unknown

The last successful observation of the unchanged `FullReal04` task was at
2026-11-04 05:41:39 UTC: 81 completed binaries, 79 passed and two failed, with
`containers` still running as PID 4964. Subsequent SSH queries returned transport
exit 255. They did not retrieve a native task exit, result count or boot time.

A verbose, read-only SSH echo reached the Windows OpenSSH 9.5 banner, sent the
client's key-exchange initialization and timed out without a server key-exchange
reply. A second echo, changing only the client's KexAlgorithms offer to
`curve25519-sha256`, received `Exceeded MaxStartups` and was disconnected before
key exchange. That refusal makes the algorithm comparison inconclusive. It does
not establish why the earlier handshakes stalled.

QEMU PID 22354 remained alive, its monitor reported `VM status: running`, and its
display showed the Windows lock screen. None of those observations establishes
the Windows boot time, native suite state or absence of an earlier host failure.
The VM and native task were not restarted. Local polling watchers were suspended
and their verified read-only SSH guard connections closed to permit a single
transport observation at a time. They were then restored with a five-minute
backoff after SSH transport failures. Local launch-phase gates keep just one
prerequisite SSH observer active at a time; native guards still establish task
completion before deployment. No candidate diagnostic was deployed.

The text views retain the relevant protocol lines. `observations.json` records
the original local capture hashes, view hashes and selection rule. Full raw logs
and the framebuffer remain local. This is an observation-channel failure, with
the native host result unresolved. It neither establishes a general WHP host
blocker nor changes the separate, unresolved HVF/VHE causal investigation.

# Reverse native TCP reset fixture

Policy cbd392/df2acf, unchanged current main33df; implementer and assigned
adversarial relay tester. This fixture extends signed5f6a8a8d4 without changing
the shared pump or TCP reset finalizer. The preceding [reset record](../reset/README.md)
contains the actual backend-reset red, correction, guard removal and restoration.

r93, private Fedora `~/relay-tcp-forward-cbd3-light-v10`, passed GNU and static
musl library/cfg(test) compilation0, followed by two exact ignored native tests0.
Each test runs direct and mediated conversations under the same original
30000ms Deadline. The backend independently receives the13-byte request and
sends the8-byte greeting, then joins while its accepted socket remains held and
its read half is still open. The client has independently received that greeting
before its owned socket is abortively closed. Both direct and mediated backends
then observe actual native `ConnectionReset`/errno104.

The mediated pump retains `Fault::Native { Client, Read, ConnectionReset, 104 }`,
13/8 transferred prefixes, zero pending/extra bytes and `Abort::Requested(Backend)`.
Option acceptance alone is not the result: the independent backend's subsequent
read supplies the reset witness. Both routes return child FDs4→4, all owned
workers join, and the parent observes held pidfd exit/reap and FD4→4. Before and
after binary hashes match. Kernel resource return remains `Unknown`; no hard
physical/kernel cap or comparative performance is established.

Exact native filter:
`managed_network::intercept::tcp_forward::native_tests::native_tcp_client_reset_matches_direct_peer`.
Static-musl binary SHA256
`44da830e9bdf17adc94032d0aa6c263f9974485469a6b9dbb121fb4029f01c3e`;
GNU `33ffed8b49ca59ecbbf7141fa5346560d824f6df07ca46f5284562c2a3b0edf8`.
Formatted guest fixture source SHA256
`ec520bc4e2bfd9aed5fe6365c7ab85c6671ffed6e8a8fc3690bf7e3bbbe1d8e7`.
Shared pump remains `ab2ac5712e25dcd7210bdde287f126eda3c8b96ed2101c93c2055629d6fe5951`.
The exact submitted [command](command.sh), source identities, inputs, raw
stdout/stderr, parsed observations and results are retained here and remotely.
The two libtest stdout files retain their original final blank lines in lossless
`stdout.raw.gz`; [raw-storage.json](raw-storage.json) maps original hashes/lengths.
Reproduction requires a NEW owned source/target path and fresh guarded admission;
the command deliberately refuses an existing root. It requires the recorded
source packet variables, not a mutable later checkout. Toolchain Rust1.98.1,
libc0.2.189, Linux6.19.10 amd64; 1CPU/1GiB/2GiB light scope, one test thread,
45000ms parent bound, no VM or namespace mutation in this batch.

R01/R04/R06/R07/R13: only focused cfg(test) code and typed observation checks;
test assertions are deliberate, no production change or new state owner.
R02/R03/R05/R12: the previously researched native zero-linger contract and
existing TCP owner apply; caller-sized slots3/5 and finite byte/time inputs,
no new manager/cache/timer/second pump. R09/R10/R15: actual independent reset,
prefix and limited cleanup witnesses, exact retained identities; this adds
reverse-direction coverage rather than a new unproven production correction.
R08/R14: ROOT owns all profiles/benchmarks; none run here. R11: GNU/musl amd64
native only; other targets, KVM reset, full normal caller/lint and option-failure
paths remain unverified. R16: no local build/target/cache, root/protected cache
write, paused WinARM touch or source/evidence/binary deletion. Old-engine host
finish and production shared consumer validation remain open independent work.

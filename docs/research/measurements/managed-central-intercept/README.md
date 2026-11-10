# Coordinator IPv4 interception evidence

Source `fed0102c8443e46ed9a15a24025eae6fff4c2ddc`, coordinator runs r197/r198
on Fedora44 amd64, Linux6.19.10/KVM, ordinary uid1000. Current policy receipt:
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Root owns these real-VM executions, independent mutations and evidence review.

## Observed effects

The host record parser passes. Each fresh local-to-peer and forwarded IPv4 TCP
guest first receives `BACKEND\0\xff` from its actual native backend. After rule
installation, each receives distinct `RELAY\0\x80` bytes from the transparent
responder, with the original destination/client tuple preserved. Actual worker
counts are backend1/relay1; both workers join. These are synthetic trusted test
roles, not a forwarding relay, untrusted principal attribution or product adoption.

Independent local mutation removes only OUTPUT marking. Independent forwarded
mutation removes the complete PREROUTING interception/mark rule. Each returns
cargo101 and observes an actual second backend accept/receipt, relay0 and joined
workers. Neither mutation relies on a missing alert as evidence. Removing only
TPROXY while leaving local routing would test different behavior and was not used.
Restoring byte-identical source, touching it and rebuilding restores both passes.
See `managed-intercept-fed0-mutations-v1/verdict.json`, patches, source hashes,
guest stdout/stderr and campaign logs. Every exit record agrees with these results.

## Reproduction and limits

Each run's `inputs.json`, `source-sha256.json`, supplied artifact association,
effective config and driver logs retain exact invocation inputs for the driver
`../managed-network-intercept/run.sh`. Admitted envelope:4CPU/8GiB/20GiB;
each test VM1vCPU/256MiB,120000ms boot/report budget,30000ms original operation
deadline; fixture mark1/table100/priority100. These are explicit finite test inputs,
not production limits or measured scheduling allowances. OS assigns ports and
interface indices. Three new guest network namespaces use RFC5737 test addresses;
host networking/root/security settings are untouched. Kernel provenance is in
`../managed-central-vm/` and each supplied-artifacts record. No production pin changed.

The transferred archive has SHA256
`d61abdb5386040c50a5fd166980b9bdd906325c57854ade5d35cdf2c88054dab`;
`SHA256SUMS` covers retained regular raw files. No images, executable artifacts,
credentials or private keys are committed.

R01/R04/R06/R07/R13: test fixture uses the reviewed finite states, checked records,
owned cancellation/joining and existing native socket/netlink seams. No production
panic-free claim follows. R02/R03/R05/R12: pinned primary sources and mechanism
alternatives are in `../managed-network-intercept/README.md` and `sources.json`;
this record supplies runtime/mutation data, without selecting final architecture,
new cache or polling. R09/R10/R15: actual byte effects and restored independent
mutations above, executed centrally. R11: actual amd64 KVM only; other hosts,
architectures, libcs and seven-target gate remain owed for later source. R08/R14:
no comparative latency/throughput, allocation, fault or resource-cost claim;
root still owes those measurements. R16: private root clone/target, admitted runner,
retained source/evidence; no user data removed.

Still unverified: loopback/self addresses, IPv6, UDP, Unix/FD transfers, real
grant attribution, hard aggregate/parent/kernel budgets, fail-closed listener or
relay death, mandatory route coverage, policy replacement and generation safety.
These two passing route experiments close none of those gaps or P4/P6 as a whole.

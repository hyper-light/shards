# The Arrival bench in main's CI (D209 Part 11)

**Question.** How does ci.yml's `firecracker comparison (x86_64)` job's Arrival step
(`cargo bench --locked -p shards --bench arrival -- --rates 2,5,10,20,40 --secs 10`) end on
main, and how long does it take when it passes?

**Method.** GitHub's records of main's CI: `history.graphql`, the firecracker check runs of
main's last 600 commits, paged 100 at a time (`gh api graphql -F n=100 [-F after=CURSOR] -f
query="$(cat history.graphql)" > fc-pageN.json`), read 2026-11-17 05:55Z at main 8d6029e4d,
and summarized by `history.py PAGES_DIR history.json > history.txt`. `history.json` is each
run as the records gave it: commit, run, job, conclusion, times and steps.

**Found** (`history.txt`). 39 firecracker jobs on main from 2026-11-14 to 2026-11-17.
- The Arrival step passed in 18: p50 100 s, p90 141 s, p99 and max 185 s (n 18).
- 11 jobs were cut at the job's 30-minute limit, each in its Arrival step, between
  2026-11-15 (1c4570e36) and 2026-11-17 (0c3213356, CI 44007524337). Each ended 2100 or
  2101 s after it began, five minutes past its limit; none ran a step after Arrival (Guest
  memory, Raw samples, the checkout's post step: still pending in the records). GitHub kept
  no log of 44007524337's (job 133788368749: BlobNotFound at 05:23Z and again at 05:48Z).
- Ten of the cut runs waited for each rate's requests with no judge; the eleventh,
  0c3213356, with b713e66b2's (`Patience::clients`). Two of the 18 passes had it.
- The other cancelled jobs (243 to 1263 s) were cut short by their run's cancellation, three
  of them in Arrival, none at its limit.
- Before Arrival, the passing jobs spent 942 to 1416 s; after it, Guest memory 22 to 35 s
  and Raw samples 1 to 2 s.

## After CI 44164945050 (95247a79c, 2026-11-17 22:00Z, goal 1453357d, own-38b)

**Question.** In CI 44164945050 the firecracker job (134333189853) ran Compare in 963 s, passed
its Envelope, and ran THP advice 129 s, Restore 15 s and Image 46 s; Arrival was cut at its own
5 minutes (313 s); Guest memory (M117), 22 to 35 s in every earlier pass, had run 293 s when the
job reached its 30 minutes, and GitHub ended the job 300 s later with no log kept (BlobNotFound
at 21:01Z and 21:39Z) and no Raw samples. Was that a change between 8247e1448 (Arrival 92 s, M117
30 s) and 95247a79c, or the host, the steps or the bench?

**Method.** The records again, through 95247a79c: own-38's pages (05:55Z, at 8d6029e4d) and, from
2fe4947fb back, 200 commits paged 25 at a time with `steps(first:40)` (21:41Z), both read by
`history.py` into `history-44164945050.json` (`history-44164945050.txt`). `hosts.py` (`hosts.txt`)
splits them by host: no record names a CPU, but job logs do (lscpu in the Host step: model and
hypervisor), and the Image step's time tells EC2's runners (Intel Xeon 6975P-C, nested KVM) from
Azure's (Hyper-V: AMD EPYCs, and an Intel Xeon Platinum 8573C in CI 44189693085); it then
derives Guest memory's and the job's limits by M560's rule. `rates.py` (`rates.txt`) reads those six logs' Arrival step.
The same run's test jobs' logs (fetched once into the lanes' ci-logs/44164945050) name their
hosts.

**Found.**
- The Image step: the logged EC2 runs (Intel Xeon 6975P-C under KVM) 45 and 45 s, the logged
  Azure runs (Hyper-V: AMD EPYC 7763, 9V45, 9V74) 53 to 73 s, and 44189693085's Azure Intel Xeon
  Platinum 8573C 63 s; no unlogged run falls between 48 and 52 s. 95247a79c's took 46 s: an EC2
  host, as both of its run's x86_64 test jobs had (their logs). 8247e1448's ran on an Azure AMD
  EPYC 9V74 (its log).
- Arrival by host: on EC2, before the judged drain 3 passes (116 to 185 s) and 8 cuts at the
  job's limit; with it 2 passes (95, 176 s) and 1 cut at its own limit (95247a79c). On Azure, before
  it 21 passes and 3 cuts at the job's limit; with it 7 passes of 7, at most 114 s.
- Rate 40 by host (`rates.txt`): the two EC2 logs failed 108 and 362 of 386 requests, the first
  with a p50 of 78 s and its host's load at 1103 as it ended; the four Azure logs failed none, at
  most 34 s. Rate 20 is quicker on EC2 (p50 0.04 s) than on Azure (0.73 to 5.11 s). Why rate 40's
  requests fail on EC2 no log says: the bench sends their output to /dev/null, and its daemon's
  log goes with its home.
- Guest memory: 33 passes, p50 28, p90 34, p99 and max 35 s; on EC2 27 to 29 s (5). It never
  before followed a cut Arrival: a cut bench's daemon, a session of its own (daemon.rs `detach`),
  outlives the step, its microVMs with it, and the job's next steps ran beside them.
- The job outside Arrival and Guest memory, in the 33 runs that passed both: p50 1091, p90 1325,
  p99 and max 1420 s. Before Arrival, EC2's took at most 1085 s without the six snapshot pairs
  (4bef86c91) and 1188 s with them (95247a79c, one run). Their cost on Azure: 1193 s of
  Compare in 44189693085 (Intel 8573C), against 1036 to 1063 s on Azure's AMD EPYC 7763 without
  them.

**Decided** (D209 Part 11 amended, ci.yml): the samples kept before Arrival as well as at the end;
a cut Arrival's daemon stopped, and its log said, before the next step; Guest memory's limit a
third above its slowest, 47 s, so 1 minute; the job's 5 + 1 + 32 = 38 minutes (a third above
1420 s is 1893 s); KVM's failed-entry VMCS kept (scripts/ci/kvm-host) and the kernel's log of OOM
kills and failed VM entries said in every run. Arrival keeps its 5 minutes: its slowest pass is
still 185 s.

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

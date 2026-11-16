# p3-warm-start: P3.1's warm start, at Shards' level

Phase 3 (kiln goal 9a2c7843, kiln plan 01a1dd2e313f72f69c6d93061143cc2e), step P3.1: "Hex takes
Shards' ImagePools", met when a "warm agent start, end to end (p50, n>=20) on KVM and HVF is at
most 300 ms, measured by committed benchmark reporting n, p50, p90, p99, max". The harness is
`warm.py`, and the record is PM M612.

It measures the call Hex makes, `Images::boot_stoppable`, through shards-api with no Hex:
- **cold**: `ImageShards`, which Hex calls today (hex-runtime guest/image.rs `host_images`). Each
  start restores its image's template (D25), and reports `StartedFrom::Restored`. This is the
  baseline now.
- **warm**: `ImagePools` (D253), which Hex calls once P3.1 lands. Each start takes a warm host
  of its pool and reports `StartedFrom::Warm`. This is the arm the bar is read from.

Both are the same image, spec and home, started by api_image's `image_start_costs` with
`SHARDS_IMAGE_COST_KINDS=Restored,Warm`. The two kinds take turns in one process (round 0 cold
then warm, round 1 warm then cold, and so on). Each start must report the kind it was asked,
or the process fails, which ends the run (AC1: a box matching a pool's exemplar starts Warm).

**Usable.** The plan's span is the agent's start "end to end". Here that is from the
`boot_stoppable` call (as `hex.image.boot` makes it, with `keep_diagnostics`) to the moment the
caller has read the member's first output: the test guest's harness prints `ready` once it
runs, and the caller reads it from the member's stdout (`take_output`). That span is reported
as `first output`, and the bar is read from it. `boot` (to the call's answer) and `stop` are
reported beside it.

**Paced warm starts.** A warm start is asked only once its pool says a host of it is warm
(`CostRig::warm_ready`, judged by the tests' `Patience`), as a caller's next start comes once
the pool has refilled the host its last start took. So the warm numbers are of starts that
found a host warm. How often a start at a given arrival rate finds none is the pool's sizing
(D26's demand rule), not this harness's measure.

## Running it

Run it from a checkout at the revision measured, on the host whose backend it measures. `DIR`
is that checkout. `warm.py` builds `api_image` there in the `release` profile (measurements
keep `release`), then measures. `OUT` must be a new directory: it keeps every process's log
(`quiet-R.log`, `loaded-R.log`) and the report (`summary.txt`), which is also printed.

**HVF** (an M-series Mac). Run it as a user's program, through launchd-run.sh: an agent's
session is clamped to utility QoS (PM M317). launchd-run.sh passes on only `PATH`, `HOME` and
`TMPDIR`, so set any other variable with `env` inside its command. The report lands in
`/tmp/p3w-hvf.log`, and the exit code in `/tmp/p3w-hvf.exit`.

```sh
docs/research/measurements/macos-sched/launchd-run.sh /tmp/p3w-hvf \
  env SHARDS_REQUIRE_VMS=1 python3 -I docs/research/measurements/p3-warm-start/warm.py "$PWD" /tmp/p3w-hvf-out
```

**KVM** (Linux x86_64 or arm64, `/dev/kvm` usable). The loaded condition keeps every CPU of
the process's affinity busy. On a shared runner, bound it with `taskset`, and record the
bound with the result:

```sh
SHARDS_REQUIRE_VMS=1 python3 -I docs/research/measurements/p3-warm-start/warm.py "$PWD" /tmp/p3w-kvm-out
# on a shared runner, its first N CPUs only:
SHARDS_REQUIRE_VMS=1 taskset -c 0-$((N - 1)) python3 -I docs/research/measurements/p3-warm-start/warm.py "$PWD" /tmp/p3w-kvm-out
```

**WHP** (Windows x86_64 or arm64, the Windows Hypervisor Platform on). Keep `CARGO_TARGET_DIR`
outside the checkout. On jupiter that is the shared `C:\Users\corph\.cache\whp-pipes\target`.
In PowerShell:

```powershell
$env:SHARDS_REQUIRE_VMS = "1"
$env:CARGO_TARGET_DIR = "C:\Users\corph\.cache\whp-pipes\target"
python -I docs\research\measurements\p3-warm-start\warm.py $PWD C:\Users\corph\.cache\p3w-whp-out
```

Under kiln's bash on jupiter, the same is
`SHARDS_REQUIRE_VMS=1 CARGO_TARGET_DIR=C:/Users/corph/.cache/whp-pipes/target python -I docs/research/measurements/p3-warm-start/warm.py "$(pwd -W)" C:/Users/corph/.cache/p3w-whp-out`.
Snapshots restore only where `vm::restores_here()` says so (not on Windows Server 2022's WHP,
PM M345). Elsewhere `image_start_costs` says SKIP, and the report marks that condition
unverified, never passed.

Options:
- `--n N`: N starts of each kind per condition, in place of the stability rule below. Under
  368, a p99 has no interval and the line says `none (too few)`. Under 20, P3.1 gets no
  verdict.
- `--budget SECONDS`: no condition starts another round past this. One that stops so is
  reported as `--budget spent, its p99s still moving`.

Nothing else is chosen: the image, its spec (1 vCPU, 512 MiB, no network, PM M593) and the
pool's room of two warm hosts and its keep-alive (`ImagePools::KEEP`, D26) are
`image_start_costs`'s own.

## What it reports

- A header: the host, its OS and release (and macOS version), its CPU count, and the revision,
  marked where the tree has uncommitted changes.
- For each condition, `quiet` (the host as it is, with nothing added) and `loaded` (one process
  spinning on each CPU the harness may run on):
  - why its sampling stopped and after how many rounds;
  - each process's busy count and load averages before and after (Windows keeps none);
  - its logs.
- Then, for each arm (cold, warm) and span (boot, first output, stop): n, p50, p90, p99 and max
  in ms, by nearest rank as `common::measured` takes them (image-start-ab's `stats`), and the
  p99's distribution-free 95% interval.
- `warm - cold, paired by round, first output`: the median of each round's warm start less its
  cold one, with its 95% interval. The two starts of a round are back to back in one process,
  so the host's drift falls on both (PM M29).
- `P3.1`: `met` where the warm first output's p50 is at most 300 ms over at least 20 starts,
  else `MISSED`, with that p50's 95% interval. `left of it for Hex's own steps` is 300 ms less
  that p50: what Hex's own work around the call (admission, its box, kiln's placement) may take
  for an end-to-end warm start to meet the bar.

The conditions take turns going first, ABBA (image-start-ab's `order`), and each samples until
its own p99s are stable.

## Where each number comes from

| Number | Source |
|---|---|
| 300 ms, n ≥ 20, p50 | P3.1's quantitative criterion (kiln plan 01a1dd2e313f72f69c6d93061143cc2e) |
| 368, the first n of each kind | the fewest samples whose p99 has a distribution-free two-sided 95% interval at all: ⌈ln(0.025) / ln(0.99)⌉ (Le Boudec, *Performance Evaluation of Computer and Communication Systems*, 2010, Theorem 2.1), as in s33-injector-ab |
| doubling, until stable | n doubles until each arm's first-output p99 lies within the interval its previous round gave (s33-injector-ab's rule) |
| 0.99 | the p99, the tail the rules report and the person's rule holds first-class |
| 0.95 | the conventional two-sided level of a distribution-free quantile interval (Le Boudec 2010, §2.4.3). It is a convention, not a measurement, and it is the same level s33-injector-ab uses |
| busy processes | one per CPU the harness may run on (its affinity, `os.sched_getaffinity` or `os.process_cpu_count`), as s33-injector-ab's `S33_AB_LOAD=cpus` |
| 6 (median) | the fewest samples whose median has a 95% interval: 0.5^6 = 1/64 is within 1/40 (the paired difference and the verdict always have more) |

The runner and statistics are image-start-ab's, imported from `../image-start-ab/ab.py`: the
cargo command, its environment, the sample lines, nearest rank, `stats`, the ABBA order and
the process runner. The interval and the stability rule are s33-injector-ab's (Rust in
shards-net's tests), carried into Python here because a Python harness cannot call them.

## Its own check

`warm_test.py` holds the statistics to their sources, with no build and no VM:

```sh
python3 -I docs/research/measurements/p3-warm-start/warm_test.py
```

It checks:
- the first n is 368, and equals its closed form;
- each interval at n = 368, 736 and 1472 (p99), and at 6, 20 and 368 (median), against exact
  binomial sums in fractions, apart from `interval`'s sums in logs;
- the median's interval needs 6 samples, and at 20 it is X(6) to X(15);
- nearest rank;
- each round's pairing, whichever kind came first, and the refusal of an unasked kind;
- the stability rule;
- the conditions' turns;
- the bar: p50 at most 300 ms, n at least 20.

Its first run is owed to ROOT, as is the build and run of the changed `image_start_costs`. The
person's rule of 2026-11-16 leaves every build and test to ROOT. Each guard removal below must
fail the check (exit 1). The file is restored after each:

```sh
D=docs/research/measurements
for e in \
  "$D/p3-warm-start/warm.py|s/tail = \(1 - CONFIDENCE\) \/ 2/tail = 1 - CONFIDENCE/" \
  "$D/p3-warm-start/warm.py|s/return \(max\(lows\), min\(highs\)\)/return (min(lows), min(highs))/" \
  "$D/p3-warm-start/warm.py|s/return \(max\(lows\), min\(highs\)\)/return (max(lows), max(highs))/" \
  "$D/p3-warm-start/warm.py|s/paired\.append\(usable\[WARM\] - usable\[COLD\]\)/paired.append(usable[COLD] - usable[WARM])/" \
  "$D/p3-warm-start/warm.py|s/lo <= p99 <= hi/lo < p99 < hi/" \
  "$D/p3-warm-start/warm.py|s/ab\.nearest\(v, 50\) <= BAR_MS/ab.nearest(v, 50) < BAR_MS/" \
  "$D/image-start-ab/ab.py|s/\(len\(v\) \* p \+ 99\) \/\/ 100/(len(v) * p) \/\/ 100/" \
  "$D/image-start-ab/ab.py|s/if r % 2 == 0 else/if True else/"
do
  f=${e%%|*}
  perl -0pi -e "${e#*|}" "$f" && python3 -I "$D/p3-warm-start/warm_test.py" >/dev/null 2>&1; echo "rc=$? ${e#*|}"
  git checkout -- "$f"
done
```

In order, the removals are:
1. the tails not halved (the first n becomes 299);
2. the interval's lower end the lowest, not the highest;
3. its upper end the highest, not the lowest;
4. the pairing reversed;
5. the stability rule made strict;
6. the bar made strict;
7. nearest rank floored;
8. the turns always in one order.

## What it does not count

- Hex's own steps. Hex's end-to-end warm start is this harness's warm start plus Hex's own
  work around the call. P3.1's final evidence is that end-to-end start, measured through Hex
  and kiln once P3.1 lands (lane phase3-v's kiln-level steps, /Users/adalundhe/g121-tmp/lanes/
  phase3-v.md section 6). This harness gives Shards' share of it, and the time left for Hex.
- A real agent's image. The image is the tests' own and its harness the test guest's `sleep`.
  An agent harness's image (Claude Code's, Codex's) is larger, and its harness does more before
  its first output.
- Misses. Every warm start here found a host warm (paced). A burst faster than the pool's
  refill starts cold.
- The pool's room is `image_start_costs`'s room of two. A Hex node's room, charged through
  `WarmRoom`, admits as many warm hosts as its free memory and vCPUs hold.

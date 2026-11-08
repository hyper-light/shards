#!/usr/bin/env python3
"""What a kernel's tick rate costs a VM's host, and what it buys a cold boot (PM M66).

    tick-cost.py --bin DIR --guest FILE --a KERNEL --b KERNEL [--runs N] [--mib M] [--idle-s S]

Each sample is a fresh `shards vm run` of the test guest (crates/testguest), the kernels
alternating A B, then B A. For each, the VM process's host CPU time (user and system, from
wait4(2)) and wall time:

- boot: `work` with nothing to do: the boot alone;
- work: `work` hashing --mib MiB (1024) of the test pattern: a busy vCPU;
- idle: `idle`, held --idle-s seconds (10) after it says it is ready, then killed.

It reports n, p50, p90, p99 and max of each, and of work less boot: what the work itself
cost the host. Samples of one iteration are paired, A's with B's: for each metric, the
median of the paired differences B - A with a bootstrap 95% interval (10,000 resamples,
seeded), as scripts/bench-envelope computes them. --only boot,work,idle runs some. The boots' own time is kernel-ab/ab.py's, which boots shards-init.
"""
import argparse, math, os, platform, random, statistics, subprocess, time


def sample(bin_dir, kernel, guest, cmdline, idle_s=None):
    """Spawn directly, so wait4 reports the VM process's own CPU time."""
    env = dict(os.environ)
    args = [os.path.join(bin_dir, "shards"), "vm", "run", "--kernel", kernel, "--init", guest,
            "--cpus", "1", "--memory", "256", "--cmdline", f"console=ttyS0 quiet panic=-1 {cmdline}"]
    r, w = os.pipe()
    start = time.monotonic()
    pid = os.posix_spawn(args[0], args, env, file_actions=[
        (os.POSIX_SPAWN_DUP2, w, 1), (os.POSIX_SPAWN_DUP2, w, 2), (os.POSIX_SPAWN_CLOSE, r)])
    os.close(w)
    text = b""
    with os.fdopen(r, "rb") as f:
        if idle_s is not None:
            while b"SHARDS-TEST READY" not in text:
                chunk = f.read1(4096)
                if not chunk:
                    break
                text += chunk
            time.sleep(idle_s)
            os.kill(pid, 9)
        text += f.read()
    _, status, usage = os.wait4(pid, 0)
    wall = time.monotonic() - start
    text = text.decode(errors="replace")
    if idle_s is None and (not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0 or "SHARDS-TEST PASS" not in text):
        raise SystemExit(f"{cmdline}: {status}\n{text}")
    return {"cpu_ms": (usage.ru_utime + usage.ru_stime) * 1e3, "wall_ms": wall * 1e3}


def pct(v, p):
    v = sorted(v)
    return v[min(max(1, math.ceil(p / 100 * len(v))), len(v)) - 1]


def main():
    ap = argparse.ArgumentParser()
    for flag in ("--bin", "--guest", "--a", "--b"):
        ap.add_argument(flag, required=True)
    ap.add_argument("--runs", type=int, default=30)
    ap.add_argument("--mib", type=int, default=1024)
    ap.add_argument("--idle-s", type=float, default=10)
    ap.add_argument("--only", default="boot,work,idle")
    a = ap.parse_args()
    kernels = {"A": a.a, "B": a.b}
    rows = {k: {} for k in kernels}
    add = lambda k, m, x: rows[k].setdefault(m, []).append(x)
    for i in range(1 + a.runs):
        for k in ("A", "B") if i % 2 == 0 else ("B", "A"):
            only = a.only.split(",")
            boot = sample(a.bin, kernels[k], a.guest, "shards_test=work shards_work_mib=0") if "boot" in only or "work" in only else None
            work = sample(a.bin, kernels[k], a.guest, f"shards_test=work shards_work_mib={a.mib}") if "work" in only else None
            idle = sample(a.bin, kernels[k], a.guest, "shards_test=idle", a.idle_s) if "idle" in only else None
            if i == 0:
                continue  # warm-up
            if boot:
                add(k, "boot_cpu_ms", boot["cpu_ms"])
            if work:
                add(k, "work_cpu_ms", work["cpu_ms"])
                add(k, "work_wall_ms", work["wall_ms"])
                add(k, "work_less_boot_cpu_ms", work["cpu_ms"] - boot["cpu_ms"])
            if idle:
                add(k, "idle_cpu_ms", idle["cpu_ms"])
    rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    print(f"host {platform.machine()} {platform.platform()} · revision {rev} · n={a.runs} · "
          f"work {a.mib} MiB · idle {a.idle_s} s")
    for k, path in kernels.items():
        print(f"{k}: {os.path.basename(os.path.dirname(os.path.abspath(path)))}/{os.path.basename(path)}")
    print(f"{'metric':<24} {'kernel':<6} {'p50':>9} {'p90':>9} {'p99':>9} {'max':>9}")
    for metric in rows["A"]:
        for k in kernels:
            v = rows[k][metric]
            print(f"{metric:<24} {k:<6} " + " ".join(f"{pct(v, p):>9.1f}" for p in (50, 90, 99, 100)))
    rng = random.Random(20260929)
    print(f"{'paired B - A':<24} {'median':>9} {'95% interval':>22}")
    for metric in rows["A"]:
        diffs = [b - x for x, b in zip(rows["A"][metric], rows["B"][metric])]
        meds = sorted(statistics.median(rng.choices(diffs, k=len(diffs))) for _ in range(10_000))
        print(f"{metric:<24} {statistics.median(diffs):>9.1f} [{meds[249]:>9.1f}, {meds[9749]:>9.1f}]")


if __name__ == "__main__":
    main()

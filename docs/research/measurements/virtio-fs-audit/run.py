#!/usr/bin/env python3
"""virtio-fs's request paths at two revisions of shards-vmm, alternating fresh processes.

    run.py OLD_REV [--runs N] [--n SAMPLES] [--delay-us D]
    run.py --old-bin OLD --new-bin NEW [...]

Builds this harness twice, in a temporary git worktree of OLD_REV and in the working tree
(NEW), each against its own crates/vmm, or takes two built binaries. For each case (open,
serve, held), runs a fresh process per arm in turn, OLD NEW then NEW OLD, after one
warm-up process each, and pools each arm's samples: n, p50, p90, p99 and max in
microseconds, and the median of the paired differences of the processes' medians
(NEW - OLD) with a bootstrap 95% interval.
"""
import argparse, json, os, platform, random, shutil, subprocess, tempfile

here = os.path.dirname(os.path.abspath(__file__))
repo = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], cwd=here, text=True).strip()
rel = os.path.relpath(here, repo)

ap = argparse.ArgumentParser()
ap.add_argument("old", nargs="?")
ap.add_argument("--old-bin")
ap.add_argument("--new-bin")
ap.add_argument("--runs", type=int, default=20)
ap.add_argument("--n", type=int, default=2000)
ap.add_argument("--delay-us", type=int, default=5000)
args = ap.parse_args()

work = tempfile.mkdtemp(prefix="virtio-fs-audit-")
env = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}
env["SHARDS_HELPERS"] = "skip"


def build(root):
    src = os.path.join(root, rel)
    if root != repo:
        shutil.copytree(here, src, dirs_exist_ok=True, ignore=shutil.ignore_patterns("target"))
    target = os.path.join(work, "target-" + ("new" if root == repo else "old"))
    subprocess.check_call(
        ["cargo", "build", "-q", "--release", "--manifest-path", os.path.join(src, "Cargo.toml"),
         "--target-dir", target], env=env)
    return os.path.join(target, "release", "virtio-fs-audit")


def q(v, p):
    v = sorted(v)
    return v[min(len(v), max(1, -(-len(v) * p // 100))) - 1]


try:
    if args.old_bin and args.new_bin:
        arms = {"old": args.old_bin, "new": args.new_bin}
    else:
        old_tree = os.path.join(work, "old")
        subprocess.check_call(["git", "worktree", "add", "-q", "--detach", old_tree, args.old], cwd=repo)
        arms = {"old": build(old_tree), "new": build(repo)}
    cases = {
        "open": ["--case", "open", "--n", str(args.n)],
        "serve": ["--case", "serve", "--n", str(args.n)],
        "held": ["--case", "held", "--n", "20", "--delay-us", str(args.delay_us)],
    }
    results = {}
    for case, flags in cases.items():
        def sample(binary):
            return json.loads(subprocess.check_output([binary, *flags], text=True))["ns"]
        for binary in arms.values():
            sample(binary)
        pooled = {name: [] for name in arms}
        medians = {name: [] for name in arms}
        for i in range(args.runs):
            for name in (("old", "new") if i % 2 == 0 else ("new", "old")):
                got = [ns / 1000 for ns in sample(arms[name])]
                pooled[name] += got
                medians[name].append(q(got, 50))
        results[case] = (pooled, medians)
finally:
    if not (args.old_bin and args.new_bin):
        subprocess.call(["git", "worktree", "remove", "--force", os.path.join(work, "old")], cwd=repo)
    shutil.rmtree(work, ignore_errors=True)

rev = subprocess.check_output(["git", "rev-parse", "--short", "HEAD"], cwd=repo, text=True).strip()
load = os.getloadavg()[0]
print(f"host {platform.machine()} {platform.platform()} · revision {rev} + working tree"
      f" against {args.old or args.old_bin} · load average {load:.1f}")
for case, (pooled, medians) in results.items():
    for name, v in pooled.items():
        print(f"{case:<5} {name:<4} n {len(v)} p50 {q(v, 50):.2f} p90 {q(v, 90):.2f}"
              f" p99 {q(v, 99):.2f} max {max(v):.2f} us")
    diffs = [n - o for n, o in zip(medians["new"], medians["old"])]
    boot = sorted(sorted(random.choices(diffs, k=len(diffs)))[len(diffs) // 2] for _ in range(2000))
    print(f"{case:<5} new - old, paired process medians: {sorted(diffs)[len(diffs) // 2]:.2f} us,"
          f" 95% [{boot[50]:.2f}, {boot[1949]:.2f}]")

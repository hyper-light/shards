#!/usr/bin/env python3
"""`GuestMemory::save` at two revisions of shards-vmm, alternating fresh processes.

    run.py OLD_REV [--runs N] [--mib M] [--every K] [--touch-all]

Builds this harness twice: in a temporary git worktree of OLD_REV, and in the working
tree (NEW), each against its own crates/vmm. Then runs a fresh process per sample,
alternating OLD NEW, then NEW OLD, after 3 warm-ups each, and reports n, p50, p90, p99
and max per arm, and the median of the paired differences (NEW - OLD) with a bootstrap
95% interval. Every process reserves and touches its RAM anew, so each save pays the
same page materialization.
"""
import argparse, json, os, platform, random, shutil, subprocess, sys, tempfile

here = os.path.dirname(os.path.abspath(__file__))
repo = subprocess.check_output(["git", "rev-parse", "--show-toplevel"], cwd=here, text=True).strip()
rel = os.path.relpath(here, repo)

ap = argparse.ArgumentParser()
ap.add_argument("old")
ap.add_argument("--runs", type=int, default=30)
ap.add_argument("--mib", type=int, default=256)
ap.add_argument("--every", type=int, default=16)
ap.add_argument("--touch-all", action="store_true")
args = ap.parse_args()

work = tempfile.mkdtemp(prefix="save-ab-")
env = {k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}


def build(root):
    src = os.path.join(root, rel)
    if root != repo:
        shutil.copytree(here, src, dirs_exist_ok=True, ignore=shutil.ignore_patterns("target"))
    target = os.path.join(work, "target-" + ("new" if root == repo else "old"))
    subprocess.check_call(
        ["cargo", "build", "-q", "--release", "--manifest-path", os.path.join(src, "Cargo.toml"),
         "--target-dir", target], env=env)
    return os.path.join(target, "release", "save-ab")


try:
    old_tree = os.path.join(work, "old")
    subprocess.check_call(["git", "worktree", "add", "-q", "--detach", old_tree, args.old], cwd=repo)
    arms = {"old": build(old_tree), "new": build(repo)}
    flags = ["--mib", str(args.mib), "--every", str(args.every)] + (["--touch-all"] if args.touch_all else [])

    def sample(binary):
        out = subprocess.check_output([binary, *flags, "--out", os.path.join(work, "memory")], text=True)
        got = json.loads(out)
        peaks.setdefault(binary, []).append(got.get("peak_kib", 0))
        return got["save_us"]

    peaks = {}
    for binary in arms.values():
        for _ in range(3):
            sample(binary)
    samples = {name: [] for name in arms}
    for i in range(args.runs):
        for name in (("old", "new") if i % 2 == 0 else ("new", "old")):
            samples[name].append(sample(arms[name]))
finally:
    subprocess.call(["git", "worktree", "remove", "--force", os.path.join(work, "old")], cwd=repo)
    shutil.rmtree(work, ignore_errors=True)


def q(v, p):
    v = sorted(v)
    return v[min(len(v), max(1, -(-len(v) * p // 100))) - 1]


rev = subprocess.check_output(["git", "rev-parse", "--short", "HEAD"], cwd=repo, text=True).strip()
print(f"host {platform.machine()} {platform.platform()} · revision {rev} + working tree against {args.old}")
print(f"{args.mib} MiB, a nonzero byte every {args.every} pages" + (", the rest touched" if args.touch_all else ", the rest untouched"))
for name, v in samples.items():
    print(f"{name:<4} n {len(v)} p50 {q(v, 50)} p90 {q(v, 90)} p99 {q(v, 99)} max {max(v)} us, peak RSS p50 {q(peaks[arms[name]], 50)} KiB")
diffs = [n - o for n, o in zip(samples["new"], samples["old"])]
meds = sorted(sorted(random.choices(diffs, k=len(diffs)))[len(diffs) // 2] for _ in range(2000))
print(f"new - old, paired: median {sorted(diffs)[len(diffs) // 2]} us, 95% [{meds[50]}, {meds[1949]}]")

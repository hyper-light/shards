#!/usr/bin/env python3
"""The harness's test (the plan's step 1): measure.py's output holds, for each build, its
builder's samples and its VM process's, and they agree. Every `guest` line parses (each of
cold.patch's fields an integer), as does every `host` and `build` line; each build has its
builder's samples, one at a step's end among them, and its VM process's; and in each build
the builder's clock and this host's agree in time order: the builder's samples' own times
(t_ms) rise as they arrive, the builder's and the host's samples arrive in the order of
their stamps (t, within the 50 ms two threads may take between stamping a line and writing
it), and the builder's clock advances as this host's does between its first sample and its
last (within a second, and 2%, of what the console's delivery adds).

    check.py OUT.txt...
"""
import sys

GUEST = ("t_ms", "current", "peak", "file", "active_file", "inactive_file", "anon", "shmem", "swap", "total",
         "free", "boot", "plugged", "asked", "most", "past_twice_used")
# Two threads stamp lines and write them (measure.py's `say`): one may write a line stamped
# before the other's it follows by this much.
RACE = 0.05


def fields(line):
    return dict(w.split("=", 1) for w in line.split()[1:] if "=" in w)


def check(path):
    bad, runs, last = [], {}, {}
    for n, line in enumerate(open(path, errors="replace"), 1):
        kind = line.split(" ", 1)[0]
        if kind not in ("guest", "host", "build") or (kind == "host" and not line.startswith("host t=")):
            continue
        f = fields(line)
        try:
            key = (f["build"], int(f["round"]))
            run = runs.setdefault(key, {"guest": [], "host": [], "ends": 0, "exit": None})
            if kind == "guest":
                t, ms = float(f["t"]), int(f["t_ms"])
                for k in GUEST:
                    int(f[k])
                if f["step"] != "-":
                    int(f["step"])
                run["ends"] += f["step_end"] == "1"
                run["guest"].append((t, ms))
            elif kind == "host":
                t = float(f["t"])
                int(f["pid"])
                sizes = [k for k in f if k not in ("t", "round", "build", "pid")]
                if not sizes:
                    raise ValueError("no sizes")
                for k in sizes:
                    int(f[k])
                run["host"].append(t)
            else:
                run["exit"] = int(f["exit"])
                float(f["s"])
                continue
        except (KeyError, ValueError) as e:
            bad.append(f"{path}:{n}: does not parse ({e!r}): {line.strip()}")
            continue
        if t < last.get(key, 0) - RACE:
            bad.append(f"{path}:{n}: stamped {last[key] - t:.3f} s before the line it follows")
        last[key] = max(last.get(key, 0), t)
    for (build, r), run in sorted(runs.items()):
        where = f"{path}: {build} round {r}"
        g = run["guest"]
        if not g or not run["host"] or not run["ends"]:
            bad.append(f"{where}: {len(g)} builder samples, {run['ends']} at a step's end, "
                       f"{len(run['host'])} of its VM process")
            continue
        if any(b[1] <= a[1] for a, b in zip(g, g[1:])):
            bad.append(f"{where}: the builder's own times do not rise as its samples arrive")
        host_span, guest_span = g[-1][0] - g[0][0], (g[-1][1] - g[0][1]) / 1000
        if abs(host_span - guest_span) > 1 + 0.02 * host_span:
            bad.append(f"{where}: the builder's clock advanced {guest_span:.3f} s, this host's {host_span:.3f} s")
        if run["exit"] is None:
            bad.append(f"{where}: no build line")
    return bad, runs


def main(paths):
    failed = False
    for path in paths:
        bad, runs = check(path)
        for b in bad:
            print(b)
        failed |= bool(bad)
        lines = sum(len(r["guest"]) for r in runs.values()), sum(len(r["host"]) for r in runs.values())
        print(f"{path}: {'FAILED' if bad else 'ok'}: {len(runs)} builds, {lines[0]} builder samples, "
              f"{lines[1]} of VM processes")
    return 1 if failed else 0


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    sys.exit(main(sys.argv[1:]))

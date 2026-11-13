import re, sys, datetime
# account.py LOG: the test step's time by phase: from the step's start to the first binary
# (build), each test binary from its "Running" line to the next (its run, any build it
# triggers included), its result and failures; the time left past the last one.
lines = open(sys.argv[1], encoding='utf-8', errors='replace').read().splitlines()
ts = lambda l: datetime.datetime.fromisoformat(l[:26]) if re.match(r'^\d{4}-\d\d-\d\dT', l) else None
start = end = None
runs = []
for i, l in enumerate(lines):
    t = ts(l)
    if t is None: continue
    body = l[29:]
    if 'Run # VM tests need the host' in body and start is None: start = t
    if 'has timed out after' in body: end = t
    m = re.search(r'Running (\S+) \(', body)
    if m: runs.append([t, m.group(1), None, '', []])
    m = re.search(r'test result: (\w+)\. (\d+) passed; (\d+) failed', body)
    if m and runs: runs[-1][2] = t; runs[-1][3] = f"{m.group(1)} {m.group(2)}p/{m.group(3)}f"
    m = re.search(r'^test (\S+) \.\.\. FAILED', body)
    if m and runs: runs[-1][4].append(m.group(1))
end = end or ts(lines[-1])
print(f"step: {start} -> {end} = {(end-start).total_seconds()/60:.1f} min")
print(f"before the first test binary (build): {(runs[0][0]-start).total_seconds()/60:.1f} min")
tot = 0.0
for k, (t, name, rt, res, failed) in enumerate(runs):
    nxt = runs[k+1][0] if k+1 < len(runs) else end
    span = (nxt - t).total_seconds()/60
    tot += span
    print(f"{span:6.1f} min  {name:40s} {res or '(no result: cut off)'}  failed={len(failed)}")
print(f"binaries' total: {tot:.1f} min")

import re, sys, datetime
# phases.py LOG: from cargo's test-release build's end, each test binary's span to the next,
# and the test phase's total to the job's last test result.
lines = open(sys.argv[1], encoding='utf-8', errors='replace').read().splitlines()
def ts(l):
    return datetime.datetime.fromisoformat(l[:26]) if re.match(r'^\d{4}-\d\d-\d\dT', l) else None
built = last = None
runs = []
for l in lines:
    t = ts(l)
    if t is None: continue
    b = l[29:]
    if 'Finished `test-release`' in b and built is None: built = t
    m = re.search(r'Running (tests/\S+) \(', b)
    if m: runs.append((t, m.group(1)))
    if 'test result:' in b: last = t
spans = {}
for k, (t, name) in enumerate(runs):
    nxt = runs[k+1][0] if k+1 < len(runs) else last
    spans[name] = (nxt - t).total_seconds() / 60
print(f"build finished {built}; tests to {last}: {(last-built).total_seconds()/60:.1f} min")
for name, m in spans.items():
    print(f"{m:6.1f} {name}")

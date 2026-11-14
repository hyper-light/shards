# phases.py DIR: PM M560's numbers from fetch.py's records. Each musl test job's Alpine step,
# from its log, split at cargo's `Finished test-release` line into the tests' build (setup
# included) and the tests (the `guest` builds tests start included); each step's setup, to
# cargo's first line; the jobs' other steps; and, as M420 did, for each run the limit cut,
# the suite's rest after the binary it was in, the most the runs that ran through took from
# that binary's start. Minutes; n, p50, p90, p99, max (nearest rank).
import json, os, re, sys, glob, collections, datetime as d
base = sys.argv[1]
ALPINE = 'On the native architecture, in Alpine'
ansi = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
iso = lambda s: d.datetime.fromisoformat(s.replace('Z', '+00:00'))
def ts(l):
    m = re.match(r'^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6})', l)
    return d.datetime.fromisoformat(m.group(1) + '+00:00') if m else None
def pct(v, p):
    v = sorted(v)
    return v[max(0, min(len(v) - 1, -(-p * len(v) // 100) - 1))]
def dist(v):
    return f"n={len(v)} p50={pct(v,50):.1f} p90={pct(v,90):.1f} p99={pct(v,99):.1f} max={max(v):.1f}" if v else 'n=0'
def objects(path):
    txt, dec, i = open(path).read(), json.JSONDecoder(), 0
    while True:
        while i < len(txt) and txt[i].isspace(): i += 1
        if i >= len(txt): return
        o, i = dec.raw_decode(txt, i)
        yield o
jobs = {}
for p in glob.glob(f'{base}/jobs/*.json'):
    for j in objects(p):
        if isinstance(j, dict) and 'name' in j: jobs[j['id']] = j
def span(s): return (iso(s['completed_at']) - iso(s['started_at'])).total_seconds() / 60
def step(j, name):
    for s in j.get('steps') or []:
        if s['name'] == name and s.get('started_at') and s.get('completed_at'): return s
def parse(path):
    start = fin = end = last = first = None; bins = []; cut = False
    for raw in open(path, encoding='utf-8', errors='replace'):
        l = ansi.sub('', raw.rstrip('\n')); t = ts(l)
        if t is None: continue
        b = l[29:]
        if start is None:
            if 'Run # VM tests need the host' in b: start = t
            continue
        if end: break
        if first is None and re.match(r'^\s*(Updating|Downloading|Downloaded|Compiling|Fresh)\b', b): first = t
        if fin is None and 'Finished `test-release`' in b: fin = t
        m = re.search(r'^\s*Running (tests/\S+) \(', b)
        if m and fin: bins.append((m.group(1), t))
        if 'has timed out after' in b: end, cut = t, True
        elif b.startswith('##[group]Run ') or b.startswith('Post job cleanup'): end = t
        else: last = t
    end = end or last
    if not (start and fin and end): return None
    m = lambda a, c: (c - a).total_seconds() / 60
    return dict(start=start, setup=m(start, first) if first else None, build=m(start, fin), tests=m(fin, end),
                cut=cut, bins=[(n, m(fin, t)) for n, t in bins])

print('== the Alpine step, by job and outcome (GitHub\'s step times)')
by = collections.defaultdict(list)
for j in jobs.values():
    s = step(j, ALPINE)
    if s and 'musl' in j['name'] and s['conclusion'] in ('success', 'failure'): by[(j['name'], s['conclusion'])].append(span(s))
for k in sorted(by): print(f'{k[0]:36s} {k[1]:8s}', dist(by[k]))

logs = {}
for j in jobs.values():
    p = f"{base}/logs/{j['id']}.log"
    if 'musl' in j['name'] and j['name'].endswith('musl') and os.path.exists(p):
        r = parse(p)
        if r: logs[j['id']] = (j, r)
for arch in ('x86_64', 'aarch64'):
    rs = [r for j, r in logs.values() if j['name'].startswith(arch)]
    full = max((len(r['bins']) for r in rs), default=0)
    through = [r for r in rs if not r['cut'] and len(r['bins']) >= full - 1]
    print(f'== {arch}-unknown-linux-musl, its test job\'s logs: {len(rs)}')
    print('  tests\' build (step start to Finished):', dist([r['build'] for r in rs]))
    print('  setup (step start to cargo\'s first line):', dist([r['setup'] for r in rs if r['setup'] is not None]))
    day = collections.defaultdict(list)
    for r in rs: day[r['start'].strftime('%m-%d')].append(r['build'])
    for k in sorted(day): print(f'    build {k}:', dist(day[k]))
    print(f'  ran through ({full} test binaries):', 'tests', dist([r['tests'] for r in through]), '| build', dist([r['build'] for r in through]))
    rest = collections.defaultdict(list)
    for r in through:
        for n, o in r['bins']: rest[n].append(r['tests'] - o)
    est = []
    for r in sorted((r for r in rs if r['cut'] and r['bins']), key=lambda r: r['start']):
        n, o = r['bins'][-1]
        if n in rest: est.append((o + max(rest[n]), r, n, o))
    print('  cut by the limit:', len([r for r in rs if r['cut']]), '; quiet at the cut (since its binary began):',
          dist([r['tests'] - o for _, r, _, o in est]))
    print('  tests, estimated for the cut runs (M420):', dist([e for e, *_ in est]))
    for e, r, n, o in sorted(est, key=lambda x: -x[0])[:3]:
        print(f"    {e:.1f}: {r['start']:%m-%d %H:%M} cut in {n} at {o:.1f}, build {r['build']:.1f}")
    # The tests' own step (a container of its own) is its setup and the tests, run by run.
    own = [(r['setup'] or 0) + r['tests'] for r in through] + [(r['setup'] or 0) + e for e, r, _, _ in est]
    print('  a tests step of its own (setup + tests, each run\'s):', dist(own))
    b = max(rs, key=lambda r: r['build'])
    jb = [j for j, r in logs.values() if r is b][0]
    print(f"  slowest build: {b['build']:.1f}, run {jb['run_id']} ({jb['head_sha'][:9]}), job {jb['id']}")

print('== the test jobs\' other steps, where gdb\'s install went through')
for name in ('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl'):
    v = []
    for j in jobs.values():
        a, g = step(j, ALPINE), step(j, "Hang watcher's gdb (x86_64)")
        if j['name'] != name or not a or a['conclusion'] not in ('success', 'failure'): continue
        if g and g['conclusion'] not in ('success', 'skipped'): continue
        v.append((sum(span(s) for s in j['steps'] if s['name'] != ALPINE and s.get('started_at') and s.get('completed_at')), j['run_id']))
    print(f'  {name}:', dist([x for x, _ in v]), '; slowest run', max(v)[1] if v else '-')

print('== x86_64-unknown-linux-gnu\'s Tests step, runs that ran through')
gb, gt = [], []
for j in jobs.values():
    s, p = step(j, 'Tests'), f"{base}/logs/{j['id']}.log"
    if j['name'] != 'x86_64-unknown-linux-gnu' or not s or s['conclusion'] not in ('success', 'failure') or not os.path.exists(p): continue
    fin = None; n = 0; cut = False
    for raw in open(p, encoding='utf-8', errors='replace'):
        l = ansi.sub('', raw.rstrip('\n')); t = ts(l)
        if t is None: continue
        if fin is None and 'Finished `test-release`' in l: fin = t
        if fin and re.search(r'^\s*Running tests/', l[29:]): n += 1
        if fin and 'has timed out after' in l: cut = True
    if fin and not cut and n >= 110:
        gb.append((fin - iso(s['started_at'])).total_seconds() / 60); gt.append((iso(s['completed_at']) - fin).total_seconds() / 60)
print('  build', dist(gb), '| tests', dist(gt))

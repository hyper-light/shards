# api_image.py DIR [BINARY]: which tests of one test binary (tests/api_image.rs by default) CI's
# x86_64 Linux jobs were in when they hung or were cut, from fetch.py's records (DIR/jobs,
# DIR/logs). libtest says `test NAME has been running for over 60 seconds` 60 s after NAME
# starts, and `test NAME ... ok|FAILED|ignored` as it ends, so each test seen past 60 s has a
# start (its warning's time, less 60 s) and, where it ended, a time. A test warned of and never
# ended before the binary's end (the next `Running`, the step's end, or the limit's cut) was
# still running there: its time stuck is from its start to that end. Minutes; n, p50, p90,
# max (nearest rank).
import collections, datetime as d, glob, json, os, re, sys
base = sys.argv[1]
binary = sys.argv[2] if len(sys.argv) > 2 else 'tests/api_image.rs'
ansi = re.compile(r'\x1b\[[0-9;]*[A-Za-z]')
def ts(l):
    m = re.match(r'^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6})', l)
    return d.datetime.fromisoformat(m.group(1) + '+00:00') if m else None
def pct(v, p):
    v = sorted(v)
    return v[max(0, min(len(v) - 1, -(-p * len(v) // 100) - 1))]
def dist(v):
    return f'n={len(v)} p50={pct(v,50):.1f} p90={pct(v,90):.1f} max={max(v):.1f}' if v else 'n=0'
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
warn = re.compile(r'^test (\S+) has been running for over 60 seconds')
done = re.compile(r'^test (\S+) \.\.\. (ok|FAILED|ignored)')
def parse(path):
    inside, start, ended, how, end, last, cut = False, {}, {}, {}, None, None, False
    for raw in open(path, encoding='utf-8', errors='replace'):
        l = ansi.sub('', raw.rstrip('\n')); t = ts(l)
        if t is None: continue
        b = l[29:].strip()
        m = re.match(r'^Running (\S+) \(', b)
        if m:
            if inside: end = t; break
            inside = m.group(1) == binary
            continue
        if not inside: continue
        if 'has timed out after' in b or 'The operation was canceled' in b:
            end, cut = t, True; break
        if b.startswith('##[group]Run ') or b.startswith('Post job cleanup') or b.startswith('test result:'):
            end = t; break
        last = t
        if (m := warn.match(b)): start[m.group(1)] = t - d.timedelta(seconds=60)
        elif (m := done.match(b)): ended[m.group(1)] = t; how[m.group(1)] = m.group(2)
    if not inside and not start: return None
    end = end or last
    mins = lambda a, c: (c - a).total_seconds() / 60
    took = {n: mins(s, ended[n]) for n, s in start.items() if n in ended}
    stuck = {n: mins(s, end) for n, s in start.items() if n not in ended and end}
    return dict(cut=cut, took=took, how=how, stuck=stuck, end=end)
rows = []
for j in jobs.values():
    if not j['name'].startswith('x86_64-unknown-linux'): continue
    p = f"{base}/logs/{j['id']}.log"
    if not os.path.exists(p) or os.path.getsize(p) == 0: continue
    r = parse(p)
    if r: rows.append((j, r))
libc = lambda j: 'musl' if 'musl' in j['name'] else 'gnu'
print(f'== {binary} in x86_64 Linux jobs\' logs: {len(rows)} ({collections.Counter(libc(j) for j, _ in rows)})')
hung = collections.defaultdict(list)
for j, r in rows:
    for n, m in r['stuck'].items(): hung[n].append((m, libc(j), r['cut'], j))
print('== still running at the binary\'s end (cut or not): test, runs (gnu/musl, cut), minutes stuck')
for n, v in sorted(hung.items(), key=lambda x: -len(x[1])):
    c = collections.Counter(x[1] for x in v)
    print(f"  {n}: {len(v)} (gnu {c['gnu']}, musl {c['musl']}, cut {sum(x[2] for x in v)}) {dist([x[0] for x in v])}")
print('== runs whose binary was cut, by the tests still running there')
combo = collections.Counter(tuple(sorted(r['stuck'])) for j, r in rows if r['cut'])
for k, c in combo.most_common(): print(f'  {c}: {", ".join(k) or "(none past 60 s)"}')
print('== tests past 60 s that ended: test, minutes, outcomes')
took = collections.defaultdict(list); outs = collections.defaultdict(collections.Counter)
for j, r in rows:
    for n, m in r['took'].items(): took[n].append(m); outs[n][r['how'][n]] += 1
for n, v in sorted(took.items(), key=lambda x: -pct(x[1], 50)):
    print(f'  {n}: {dist(v)} {dict(outs[n])}')
print('== by day, cut runs and their stuck tests')
day = collections.defaultdict(collections.Counter)
for j, r in rows:
    for n in r['stuck']: day[r['end'].strftime('%m-%d')][n] += 1
for k in sorted(day): print(f'  {k}:', dict(day[k].most_common()))

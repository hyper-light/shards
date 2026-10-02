"""Summarizes ab.sh's DIR/ab-*.txt (PM M80): n, p50, p90 and max of each measure, before and after, and of each alloc-phase line: python3 phases.py DIR."""
import re, sys
S = sys.argv[1] if len(sys.argv) > 1 else '.'

def runs(path):
    out, cur = [], None
    for line in open(path):
        if line.startswith('==='):
            cur = {'phases': []}
            out.append(cur)
            continue
        m = re.match(r'\s+([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys', line)
        if m:
            cur['real'], cur['user'], cur['sys'] = map(float, m.groups())
            cur['cpu'] = cur['user'] + cur['sys']
        for key, pat in [('maxrss_mb', r'(\d+)\s+maximum resident'), ('minflt', r'(\d+)\s+page reclaims'),
                         ('majflt', r'(\d+)\s+page faults'), ('footprint_mb', r'(\d+)\s+peak memory footprint')]:
            m = re.match(r'\s+' + pat, line)
            if m:
                v = int(m.group(1))
                cur[key] = v / 1e6 if key.endswith('_mb') else v
        m = re.match(r'#6 DONE ([\d.]+)s', line)
        if m:
            cur['step_add_s'] = float(m.group(1))
        m = re.match(r'#7 DONE ([\d.]+)s', line)
        if m:
            cur['step_export_s'] = float(m.group(1))
        if line.startswith('alloc-phase'):
            parts = line.split()
            d = {k: float(v) for k, v in (p.split('=') for p in parts[2:])}
            cur['phases'].append((parts[1], d))
    return out

def q(xs, p):
    xs = sorted(xs)
    if not xs:
        return float('nan')
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)

def row(name, xs, fmt='{:.2f}'):
    if not xs:
        return
    print(f'| {name} | {len(xs)} | ' + ' | '.join(fmt.format(v) for v in (q(xs, .5), q(xs, .9), max(xs))) + ' |')

for kind in ['', '-count']:
    print(f'\n## whole build{" (counting allocator)" if kind else ""}')
    for v in ['before', 'after']:
        rs = runs(f'{S}/ab-{v}{kind}.txt')
        print(f'\n### {v}\n| measure | n | p50 | p90 | max |\n|---|---|---|---|---|')
        for key in ['real', 'user', 'sys', 'cpu', 'maxrss_mb', 'footprint_mb', 'minflt', 'majflt', 'step_add_s', 'step_export_s']:
            row(key, [r[key] for r in rs if key in r], '{:.0f}' if key in ('minflt', 'majflt') else '{:.2f}')

print('\n## per phase (counting allocator), p50 over runs [p90, max for ms/peak]')
for v in ['before', 'after']:
    rs = runs(f'{S}/ab-{v}-count.txt')
    print(f'\n### {v} (n={len(rs)})\n| phase | wall ms | user ms | sys ms | allocs | reallocs | requested MB | live MB at end | peak MB (p50/p90/max) | minflt | majflt |\n|---|---|---|---|---|---|---|---|---|---|---|')
    names = [n for n, _ in rs[0]['phases']]
    for i, name in enumerate(names):
        col = lambda k: [r['phases'][i][1][k] for r in rs if len(r['phases']) > i]
        peak = col('peak_mb')
        print(f"| {i}:{name} | {q(col('ms'), .5):.1f} [{q(col('ms'), .9):.0f}, {max(col('ms')):.0f}] | {q(col('user_ms'), .5):.1f} | {q(col('sys_ms'), .5):.1f} | {q(col('allocs'), .5):.0f} | {q(col('reallocs'), .5):.0f} | {q(col('requested_mb'), .5):.1f} | {q(col('live_mb'), .5):.1f} | {q(peak, .5):.1f}/{q(peak, .9):.1f}/{max(peak):.1f} | {q(col('minflt'), .5):.0f} | {q(col('majflt'), .5):.0f} |")

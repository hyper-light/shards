#!/usr/bin/env python3
"""Where restored guests run until their first beat (PM M38).

Reads the Firecracker comparison's output with diagnostics.patch applied, and the guest
kernel's text symbols (`llvm-nm -n --defined-only vmlinux`, types T/t/W/w: "ADDR NAME"
per line). For each variant (shards' snapshot, its per-page copy, Firecracker's) it
prints the median first beat, the exit time by reason, the functions most exits came
from, and how many exits had their RIP in the kernel's crypto self-tests (the
multi-precision arithmetic under RSA, and the self-test code itself).

  profile.py SYMBOLS LOG... [--top N]
"""
import bisect
import collections
import re
import statistics
import sys

WARMUP = 3
CRYPTO = ('mpi', 'rsa', 'crypto_', 'alg_test', 'cryptomgr', 'pkcs1', 'ecc', 'dh_')


def main():
    args = sys.argv[1:]
    top = 12
    if '--top' in args:
        i = args.index('--top')
        top = int(args[i + 1])
        del args[i:i + 2]
    symbols, logs = args[0], args[1:]
    table = sorted((int(a, 16), n) for a, n in (line.split() for line in open(symbols)))
    starts = [a for a, _ in table]

    def name(addr):
        i = bisect.bisect_right(starts, addr) - 1
        return table[i][1] if i >= 0 else hex(addr)

    for log in logs:
        lines = [re.sub(r'^\d{4}-\d\d-\d\dT[\d:.]+Z ', '', l.rstrip('\n')) for l in open(log, errors='replace')]
        host = next((m.group(1) for l in lines for m in [re.search(r'"host":"([^"]*)"', l)] if m), '?')
        samples, current, per_page = [], None, False
        for l in lines:
            if l.startswith('per-page restore:'):
                per_page = True
                continue
            m = re.match(r'kvm-trace "([^"]*)"', l)
            if m:
                variant = 'firecracker' if 'firecracker' in m.group(1) else ('per-page' if per_page else 'shards')
                if variant != 'firecracker':
                    per_page = False
                current = {'variant': variant, 'rips': collections.Counter(), 'reasons': {}}
                samples.append(current)
                continue
            if current is None:
                continue
            m = re.match(r'rip (\S+) (0x[0-9a-f]+) (\d+)$', l)
            if m:
                current['rips'][(m.group(1), name(int(m.group(2), 16)))] += int(m.group(3))
                continue
            m = re.match(r'(\S+) n=(\d+) us=(\d+)$', l)
            if m:
                current['reasons'][m.group(1)] = (int(m.group(2)), int(m.group(3)))
                continue
            m = re.match(r'restore-timeline "[^"]*" first beat at \+(\d+) us', l)
            if m:
                current['beat'] = int(m.group(1))
        print('==', host, log)
        by_variant = collections.defaultdict(list)
        for s in samples:
            if 'beat' in s:
                by_variant[s['variant']].append(s)
        for variant, ss in by_variant.items():
            ss = ss[WARMUP:]
            if not ss:
                continue
            beat = statistics.median(s['beat'] for s in ss)
            reasons = collections.Counter()
            for s in ss:
                for reason, (_, us) in s['reasons'].items():
                    if reason != 'guest':
                        reasons[reason] += us
            rips = collections.Counter()
            for s in ss:
                rips.update(s['rips'])
            crypto = [sum(c for (_, f), c in s['rips'].items() if f.startswith(CRYPTO)) for s in ss]
            exits = [sum(s['rips'].values()) for s in ss]
            print('  %-11s n=%d first beat p50 %.1f ms; exits with a crypto RIP in %d of %d restores, %.1f of %.1f per restore'
                  % (variant, len(ss), beat / 1000, sum(1 for c in crypto if c), len(ss),
                     sum(crypto) / len(ss), sum(exits) / len(ss)))
            print('    exit time per restore:', ', '.join('%s %.0f us' % (r, us / len(ss)) for r, us in reasons.most_common(5)))
            for (reason, function), count in rips.most_common(top):
                print('    %6.1f %-8s %s' % (count / len(ss), reason, function))


if __name__ == '__main__':
    main()

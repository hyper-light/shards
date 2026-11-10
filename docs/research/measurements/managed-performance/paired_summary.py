#!/usr/bin/env python3
"""Retained paired samples: complete versus missing transport state stays explicit."""
from pathlib import Path
import collections
import json
import sys
from summarize import load, fields, distribution

root = Path(sys.argv[1])
groups = []
for name in ['baseline-counted', 'candidate-counted', 'baseline-system', 'candidate-system']:
    directory = root / name
    grouped = collections.defaultdict(list)
    for path in sorted(directory.glob('r*-t*-*.tsv')):
        run = load(path)
        expected_counting = name.endswith('-counted')
        assert run['config']['count_alloc'] == str(expected_counting).lower(), (path, run['config'])
        assert run['resources']['initial']['fds'] == run['resources']['closed']['fds']
        assert run['resources']['initial']['tasks'] == run['resources']['closed']['tasks']
        snapshots = collections.defaultdict(list)
        for line in path.read_text().splitlines():
            if line.startswith('transport\t'):
                snapshots[line.split('\t')[1]].append(fields(line))
        run['transport'] = {}
        for phase, rows in snapshots.items():
            assert len(set(row['connection'] for row in rows)) == len(rows)
            expected = 1 if run['config']['variant'] == 'shared' else run['config']['channels']
            assert len(rows) == expected, (path, phase, len(rows), expected)
            known = [row for row in rows if row['status'] == 'known']
            run['transport'][phase] = dict(known_connections=len(known), unknown_connections=expected-len(known),
                complete=len(known)==expected,
                known_subtotal={key:sum(row[key] for row in known) for key in ['tx_datagrams','tx_bytes','rx_datagrams','rx_bytes','lost_packets','crypto_tx','crypto_rx']})
        run['parent_after'] = json.loads(path.with_suffix('.parent-after.json').read_text())
        grouped[(run['config']['tenants'],run['config']['mode'],run['config']['variant'])].append(run)
    for (tenants, mode, variant), runs in sorted(grouped.items()):
        metrics = {}
        for phase in ['initial','servers_bound','tls_subscribed','workload','closed']:
            for key in ['wall_ns','alloc','realloc','free','allocated_bytes','live_bytes','live_delta','peak_bytes','minor_faults','major_faults','user_us','system_us']:
                metrics[phase+'.'+key] = (dict(n=0,status='unmeasured_System_allocator') if name.endswith('-system') and key in ['alloc','realloc','free','allocated_bytes','live_bytes','live_delta','peak_bytes'] else distribution([run['phases'][phase][key] for run in runs]))
            for key in ['fds','tasks','Rss_KiB','Pss_KiB','Private_Clean_KiB','Private_Dirty_KiB']:
                metrics[phase+'.'+key] = distribution([run['resources'][phase][key] for run in runs])
        metrics['first_record_ack_ns'] = distribution([s['app_ack_ns'] for run in runs for s in run['samples'] if s['record']==0])
        metrics['established_record_ack_ns'] = distribution([s['app_ack_ns'] for run in runs for s in run['samples'] if s['record']>0])
        metrics['all_record_ack_ns'] = distribution([s['app_ack_ns'] for run in runs for s in run['samples']])
        metrics['cancel_waiter_ns'] = distribution([c['waiter_ns'] for run in runs for c in run['cancel']])
        metrics['acknowledged_payload_bytes_per_s'] = distribution([len(run['samples'])*run['config']['bytes']*1e9/run['phases']['workload']['wall_ns'] for run in runs])
        for phase in ['tls_subscribed','workload']:
            metrics['transport.'+phase] = {
                'known_connections': distribution([run['transport'][phase]['known_connections'] for run in runs]),
                'unknown_connections': distribution([run['transport'][phase]['unknown_connections'] for run in runs]),
                'complete_snapshots': sum(run['transport'][phase]['complete'] for run in runs),
                'complete_totals': {key:distribution([run['transport'][phase]['known_subtotal'][key] for run in runs if run['transport'][phase]['complete']]) for key in ['tx_bytes','rx_bytes','tx_datagrams','rx_datagrams','lost_packets','crypto_tx','crypto_rx']},
                'known_subtotals': [run['transport'][phase] for run in runs],
            }
        metrics['parent_python_maxrss_KiB'] = distribution([run['parent_after']['maxrss_kib'] for run in runs])
        groups.append(dict(source=name,tenants=tenants,mode=mode,variant=variant,processes=len(runs),metrics=metrics))
print(json.dumps(dict(groups=groups,method='nearest rank; five counted or three System processes; pooled records remain clustered; CPU/fault/peak counters cumulative; QUIC stats report prepared/sent protocol datagrams, not packet-capture evidence; no warm reconnect result'),indent=2))

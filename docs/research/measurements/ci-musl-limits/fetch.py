# fetch.py DIR [SINCE [LOGS_SINCE]]: CI's records for PM M560, through `gh` (logged in): the CI
# workflow's runs created since SINCE (GitHub lists at most the newest 1000), each run's musl
# and x86_64-unknown-linux-gnu jobs with their steps' times (DIR/jobs/RUN.json), and the job
# logs (DIR/logs/JOB.log) of the test jobs whose Alpine or Tests step ran over 20 minutes:
# x86_64 musl's all, aarch64 musl's and x86_64 gnu's from LOGS_SINCE.
import json, os, subprocess, sys, datetime as d
from concurrent.futures import ThreadPoolExecutor
out = sys.argv[1]
since = sys.argv[2] if len(sys.argv) > 2 else '2026-11-08'
logs_since = sys.argv[3] if len(sys.argv) > 3 else '2026-11-12T06:00:00Z'
repo = 'repos/hyper-light/shards'
os.makedirs(f'{out}/jobs', exist_ok=True)
os.makedirs(f'{out}/logs', exist_ok=True)
def gh(*a):
    return subprocess.run(['gh', 'api', *a], capture_output=True, text=True).stdout
runs = [json.loads(l) for l in gh('--paginate', f'{repo}/actions/workflows/ci.yml/runs?per_page=100&created=>={since}',
        '--jq', '.workflow_runs[] | {id, run_started_at, updated_at, status}').splitlines() if l.strip()]
t = lambda s: d.datetime.fromisoformat(s.replace('Z', '+00:00'))
ids = [r['id'] for r in runs if r['status'] == 'completed' and (t(r['updated_at']) - t(r['run_started_at'])).total_seconds() > 1800]
def jobs(i):
    p = f'{out}/jobs/{i}.json'
    if not os.path.exists(p):
        j = gh('--paginate', f'{repo}/actions/runs/{i}/jobs?per_page=100&filter=latest', '--jq',
               '.jobs[] | select(.name | test("musl|^x86_64-unknown-linux-gnu$"))')
        open(p, 'w').write(j)
    return p
with ThreadPoolExecutor(8) as ex:
    paths = list(ex.map(jobs, ids))
long = []
for p in paths:
    for l in open(p):
        if not l.strip(): continue
        j = json.loads(l)
        if j['name'] not in ('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl', 'x86_64-unknown-linux-gnu'):
            continue
        for s in j.get('steps') or []:
            if (j['name'] == 'x86_64-unknown-linux-musl' or (s.get('started_at') or '') >= logs_since) and s['name'] in ('On the native architecture, in Alpine', 'Tests') and s.get('started_at') and s.get('completed_at') \
                    and (t(s['completed_at']) - t(s['started_at'])).total_seconds() > 1200:
                long.append(j['id'])
def log(i):
    p = f'{out}/logs/{i}.log'
    if not os.path.exists(p) or os.path.getsize(p) == 0:
        with open(p, 'w') as f:
            subprocess.run(['gh', 'api', '--allow-escape-sequences', f'{repo}/actions/jobs/{i}/logs'], stdout=f)
with ThreadPoolExecutor(8) as ex:
    list(ex.map(log, sorted(set(long))))
print(f'{len(ids)} runs, {len(set(long))} logs')

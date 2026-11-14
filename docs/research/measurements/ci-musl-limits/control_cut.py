# control_cut.py DIR: PM M563's tally. DIR holds runs.tsv (run id, sha, created, conclusion),
# jobs.tsv (run id, job id, name, conclusion) and logs/JOB.log, CI's x86_64 Linux test jobs
# that failed, fetched with:
#   gh api --paginate 'repos/hyper-light/shards/actions/workflows/ci.yml/runs?per_page=100&created=>=SINCE' \
#     --jq '.workflow_runs[]|select(.status=="completed")|[.id,.head_sha[0:9],.created_at,.conclusion]|@tsv'
#   gh api 'repos/hyper-light/shards/actions/runs/RUN/jobs?per_page=100&filter=latest' \
#     --jq '.jobs[]|select(.name|test("x86_64-unknown-linux"))|[.run_id,.id,.name,.conclusion]|@tsv'
#   gh api --allow-escape-sequences repos/hyper-light/shards/actions/jobs/JOB/logs  (colours stripped)
# For each log holding either line, by run date: how often a run host or daemon said its guest's
# signal connection was lost to "invalid data", and a run "the guest stopped before the command
# ended", and the tests that failed in the same test binary.
import collections, glob, os, re, sys
d = sys.argv[1]
runs = {l.split('\t')[0]: l.rstrip('\n').split('\t') for l in open(f'{d}/runs.tsv')}
jobs = {l.split('\t')[1]: l.rstrip('\n').split('\t') for l in open(f'{d}/jobs.tsv')}
rows = []
for p in glob.glob(f'{d}/logs/*.log'):
    j = os.path.basename(p)[:-4]
    text = [re.sub(r'^\S+Z ', '', l) for l in open(p, errors='replace').read().splitlines()]
    lost = sum('signal connection is lost: invalid data' in l for l in text)
    stopped = sum('the guest stopped before the command ended' in l for l in text)
    if not (lost or stopped):
        continue
    binary, failed = None, collections.defaultdict(set)
    for l in text:
        m = re.match(r'\s+Running (tests/\S+)', l)
        if m:
            binary = m.group(1)
        m = re.match(r'test (\S+) \.\.\. FAILED', l)
        if m and binary:
            failed[binary].add(m.group(1))
    run = jobs[j][0]
    _, sha, created, _ = runs[run]
    hit = {b: sorted(t) for b, t in failed.items() if b in ('tests/api_image.rs', 'tests/managed_output.rs', 'tests/agent.rs', 'tests/containers.rs')}
    rows.append((created, sha, j, jobs[j][2], lost, stopped, hit))
for r in sorted(rows):
    print(*r[:6], r[6])

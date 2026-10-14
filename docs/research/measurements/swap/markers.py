"""Where a run's swap start goes, in its guest (D159, docs/research/platform-measurements.md
M230): markers.apply.py has init write control-page markers 7 (its swap begins), 8 (its
header written) and 9 (swapped on), and the VMM keep markers up to 15; a release build of
that tree is SHARDS. Then N runs of `run --rm -m 64m IMAGE exit 0` in HOME, which holds
swap.py's image, each split by SHARDS_TIMING's markers, whose times are the VMM's: from
the request to marker 7, 7 to 8, 8 to 9, 9 to the workload's start (marker 4).

    python3 markers.py SHARDS HOME N
"""
import json, os, statistics, subprocess, sys
shards, home, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
env = dict(os.environ, SHARDS_HOME=home, SHARDS_TIMING="1")
def pct(xs,p):
    xs=sorted(xs); k=(len(xs)-1)*p; lo=int(k); hi=min(lo+1,len(xs)-1); return xs[lo]+(xs[hi]-xs[lo])*(k-lo)
rows=[]
for i in range(n):
    d=subprocess.run([shards,"run","--rm","--pull","never","-m","64m","swap-bench:1","exit","0"],env=env,capture_output=True)
    line=next(l for l in d.stderr.decode().splitlines() if l.startswith("shards-timing "))
    t=json.loads(line.split(" ",1)[1]); m=dict((a,b) for a,b in t["markers"])
    rows.append((t["request_us"], m))
def show(name, xs): print(f"{name}: n={len(xs)} p50={pct(xs,.5):.0f} p90={pct(xs,.9):.0f} p99={pct(xs,.99):.0f} max={max(xs):.0f}")
show("request->7 (swap begins)", [m[7]-r for r,m in rows if 7 in m])
show("7->8 header", [m[8]-m[7] for r,m in rows if 8 in m and 7 in m])
show("8->9 swapon", [m[9]-m[8] for r,m in rows if 9 in m and 8 in m])
show("9->4 rest to workload start", [m[4]-m[9] for r,m in rows if 4 in m and 9 in m])
show("request->4 workload start", [m[4]-r for r,m in rows if 4 in m])

"""M74 (docs/research/platform-measurements.md): what plain files, which guests serve with
DAX, save a VM against inline tails, which the guest's page cache copies.

ROOT holds v1 and v2, each a build's `shards`, `shardsd` and `shards-vm` (signed as
scripts/hvf-run signs them) and a `home` with python:3.13-slim pulled and its template
saved by a first run. Each run restores the template with `shards vm restore`, whose
process is the VM's, imports 24 standard modules, prints the import's time and waits on
stdin while `footprint` reads the process's physical footprint. Builds alternate by
block, one build's VMs at a time (M72); the first run of a block is dropped.

    python3 dax.py ROOT BLOCKS PER_BLOCK
"""
import glob, os, re, subprocess, sys
root, blocks, per = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
mods = "asyncio,json,email.mime.multipart,http.client,http.server,xml.etree.ElementTree,decimal,sqlite3,unittest,argparse,logging,ssl,urllib.request,csv,tarfile,zipfile,difflib,pydoc,inspect,typing,dataclasses,pathlib,subprocess,concurrent.futures"
code = ("import time,sys;t=time.perf_counter();import " + mods +
        ";print('ready',int((time.perf_counter()-t)*1e6),flush=True);sys.stdin.readline()")
def template(home):
    return [d for d in glob.glob(home + "/templates/*") if os.path.isdir(d)]
res = {"v1": [], "v2": []}
for b in range(blocks):
    for name in (["v1", "v2"] if b % 2 == 0 else ["v2", "v1"]):
        home = f"{root}/{name}/home"
        # the python image's template: the largest
        t = max(template(home), key=lambda d: sum(os.path.getsize(f) for f in glob.glob(d + "/**/*", recursive=True) if os.path.isfile(f)))
        for i in range(per + 1):
            p = subprocess.Popen([f"{root}/{name}/shards", "vm", "restore", t, "-i", "--", "python3", "-c", code],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, cwd=root)
            line = p.stdout.readline().split()
            fp = subprocess.run(["footprint", "-p", str(p.pid)], capture_output=True, text=True).stdout
            m = re.search(r"phys_footprint:\s*([\d.]+)\s*([KMG]B)", fp) or re.search(r"Footprint:\s*([\d.]+)\s*([KMG]B)", fp)
            mb = float(m.group(1)) * {"KB": 1/1024, "MB": 1, "GB": 1024}[m.group(2)] if m else float("nan")
            p.stdin.write("\n"); p.stdin.flush(); p.wait()
            if i == 0 or len(line) < 2: continue
            res[name].append((int(line[1]) / 1000, mb))
def q(v, f): v = sorted(v); return v[min(len(v) - 1, int(f * len(v)))]
for name, v in res.items():
    imp = [a for a, _ in v]; fp = [b for _, b in v]
    print(f"{name} n {len(v)} import ms p50 {q(imp,.5):.1f} p90 {q(imp,.9):.1f} max {max(imp):.1f} | footprint MB p50 {q(fp,.5):.1f} p90 {q(fp,.9):.1f} max {max(fp):.1f}")
print("load %.2f" % os.getloadavg()[0])

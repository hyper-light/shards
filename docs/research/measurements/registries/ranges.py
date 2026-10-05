"""How fast one blob arrives over N parallel ranged connections: the first BYTES of the
largest layer of IMAGE (Docker Hub, linux/arm64 unless PLATFORM), split into N equal
ranges fetched at once, for N in 1, 2, 4, 8, 16, ROUNDS rounds in turn; each its MB/s.
Hub's blob GETs redirect to its CDN, which is what is measured.

    ranges.py IMAGE [BYTES] [ROUNDS]
"""
import json, os, statistics, sys, threading, time, urllib.request

image = sys.argv[1]
size = int(sys.argv[2]) if len(sys.argv) > 2 else 512 << 20
rounds = int(sys.argv[3]) if len(sys.argv) > 3 else 3
platform = os.environ.get("PLATFORM", "linux/arm64").split("/")
repo, tag = (image.split(":") + ["latest"])[:2]
if "/" not in repo:
    repo = "library/" + repo
token = json.load(urllib.request.urlopen(
    f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"))["token"]
def get(url, accept):
    r = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}", "Accept": accept})
    return json.load(urllib.request.urlopen(r))
base = f"https://registry-1.docker.io/v2/{repo}"
idx = get(f"{base}/manifests/{tag}", "application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json")
if "manifests" in idx:
    d = [m for m in idx["manifests"] if m.get("platform", {}).get("os") == platform[0] and m["platform"].get("architecture") == platform[1]][0]["digest"]
    man = get(f"{base}/manifests/{d}", "application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json")
else:
    man = idx
layer = max(man["layers"], key=lambda l: l["size"])
size = min(size, layer["size"])
# The CDN URL the registry redirects to, signed for a while.
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a): return None
opener = urllib.request.build_opener(NoRedirect)
try:
    opener.open(urllib.request.Request(f"{base}/blobs/{layer['digest']}", headers={"Authorization": f"Bearer {token}"}))
    raise SystemExit("no redirect")
except urllib.error.HTTPError as e:
    cdn = e.headers["Location"]
print(f"{image}: largest layer {layer['size']/1e6:.0f} MB; fetching its first {size/1e6:.0f} MB; CDN {cdn.split('/')[2]}")
def fetch(lo, hi, got):
    r = urllib.request.Request(cdn, headers={"Range": f"bytes={lo}-{hi}"})
    with urllib.request.urlopen(r) as resp:
        n = 0
        while True:
            b = resp.read(1 << 20)
            if not b: break
            n += len(b)
    got.append(n)
results = {n: [] for n in (1, 2, 4, 8, 16)}
for r in range(rounds):
    for n in (results if r % 2 == 0 else reversed(list(results))):
        step = size // n
        got, threads = [], []
        t = time.time()
        for i in range(n):
            lo = i * step; hi = size - 1 if i == n - 1 else lo + step - 1
            th = threading.Thread(target=fetch, args=(lo, hi, got)); th.start(); threads.append(th)
        for th in threads: th.join()
        el = time.time() - t
        assert sum(got) == size, (sum(got), size)
        results[n].append(size / el / 1e6)
for n, v in results.items():
    print(f"{n:2d} connections: {statistics.median(v):6.1f} MB/s median of {len(v)} ({', '.join(f'{x:.0f}' for x in v)})")

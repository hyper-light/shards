"""What one ranged request for a blob costs before its first byte, over kept-alive
connections: asked of Docker Hub, which redirects to its CDN, against asked of the CDN's
URL directly. ROUNDS of each, in turn; p50, p90 and max in ms.

    hops.py IMAGE [ROUNDS]
"""
import http.client, json, os, statistics, sys, time, urllib.parse, urllib.request

image = sys.argv[1]
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 30
platform = os.environ.get("PLATFORM", "linux/arm64").split("/")
repo, tag = (image.split(":") + ["latest"])[:2]
if "/" not in repo:
    repo = "library/" + repo
token = json.load(urllib.request.urlopen(
    f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"))["token"]
hub = http.client.HTTPSConnection("registry-1.docker.io")
def get(path, accept):
    hub.request("GET", path, headers={"Authorization": f"Bearer {token}", "Accept": accept})
    r = hub.getresponse()
    return json.loads(r.read())
types = "application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json"
man = get(f"/v2/{repo}/manifests/{tag}", types)
if "manifests" in man:
    d = [m for m in man["manifests"] if m.get("platform", {}).get("os") == platform[0] and m["platform"].get("architecture") == platform[1]][0]["digest"]
    man = get(f"/v2/{repo}/manifests/{d}", types)
layer = max(man["layers"], key=lambda l: l["size"])
blob = f"/v2/{repo}/blobs/{layer['digest']}"
cdn = None
def via_hub():
    global cdn
    hub.request("GET", blob, headers={"Authorization": f"Bearer {token}", "Range": "bytes=0-0"})
    r = hub.getresponse(); r.read()
    assert r.status in (301, 302, 307, 308), r.status
    cdn = r.getheader("Location")
    return direct()
conns = {}
def direct():
    u = urllib.parse.urlsplit(cdn)
    c = conns.setdefault(u.netloc, http.client.HTTPSConnection(u.netloc))
    c.request("GET", u.path + ("?" + u.query if u.query else ""), headers={"Range": "bytes=0-0"})
    r = c.getresponse(); r.read()
    assert r.status == 206, r.status
via_hub()  # connections opened, before anything is timed
res = {"via the registry": [], "the CDN's URL": []}
for i in range(rounds):
    for name, f in (list(res.items()) if i % 2 == 0 else reversed(list(res.items()))):
        t = time.perf_counter(); (via_hub if name == "via the registry" else direct)()
        res[name].append((time.perf_counter() - t) * 1e3)
print(f"{image}: {layer['digest'][:19]}, CDN {urllib.parse.urlsplit(cdn).netloc}")
for name, v in res.items():
    v.sort(); q = lambda p: v[min(len(v) - 1, int(p * len(v)))]
    print(f"{name:17} n={len(v)} p50 {q(.5):6.1f} ms  p90 {q(.9):6.1f}  max {v[-1]:6.1f}")

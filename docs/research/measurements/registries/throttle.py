"""How soon a throttled registry answers again: requests a manifest over one kept-alive
connection until a 429, then asks again after a delay (rotating through DELAYS) and
records whether that succeeded. ECR Public by default.

    throttle.py [THROTTLES] [HOST REPO TAG]
"""
import http.client, json, sys, time, urllib.request
n = int(sys.argv[1]) if len(sys.argv) > 1 else 40
host, repo, tag = (sys.argv[2:5] if len(sys.argv) > 4 else ("public.ecr.aws", "docker/library/redis", "7.4"))
token = json.load(urllib.request.urlopen(f"https://{host}/token/?scope=repository:{repo}:pull"))["token"]
conn = http.client.HTTPSConnection(host)
headers = {"Authorization": f"Bearer {token}", "Accept": "application/vnd.oci.image.index.v1+json"}
def ask():
    conn.request("HEAD", f"/v2/{repo}/manifests/{tag}", headers=headers)
    r = conn.getresponse(); r.read()
    return r.status, r.getheader("retry-after")
DELAYS = [0, 0.01, 0.025, 0.05, 0.1, 0.25]
seen, sent = [], 0
while len(seen) < n:
    status, retry_after = ask(); sent += 1
    if status != 429:
        continue
    d = DELAYS[len(seen) % len(DELAYS)]
    time.sleep(d)
    again, _ = ask(); sent += 1
    seen.append((d, again, retry_after))
print(f"{sent} requests, {n} throttled")
for d in DELAYS:
    got = [a for x, a, _ in seen if x == d]
    print(f"retry after {d*1000:5.0f} ms: {sum(a == 200 for a in got)}/{len(got)} answered 200")
print("Retry-After sent:", sorted({r for *_, r in seen if r}) or "never")

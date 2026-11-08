"""The request rate a registry sustains anonymously: manifest HEADs at fixed rates over
one kept-alive connection, for SECONDS each, and the share answered 429. ECR Public by
default.

    rates.py [SECONDS] [HOST REPO TAG]
"""
import http.client, json, sys, time, urllib.request
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 8
host, repo, tag = (sys.argv[2:5] if len(sys.argv) > 4 else ("public.ecr.aws", "docker/library/redis", "7.4"))
token = json.load(urllib.request.urlopen(f"https://{host}/token/?scope=repository:{repo}:pull"))["token"]
conn = http.client.HTTPSConnection(host)
headers = {"Authorization": f"Bearer {token}", "Accept": "application/vnd.oci.image.index.v1+json"}
for rate in (1, 2, 4, 8, 16):
    time.sleep(3)  # let the bucket refill between rates
    start, sent, throttled = time.time(), 0, 0
    while time.time() - start < secs:
        due = start + sent / rate
        if due > time.time():
            time.sleep(due - time.time())
        conn.request("HEAD", f"/v2/{repo}/manifests/{tag}", headers=headers)
        r = conn.getresponse(); r.read()
        sent += 1; throttled += r.status == 429
    took = time.time() - start
    print(f"{rate:3d}/s asked, {sent/took:5.1f}/s sent: {throttled}/{sent} throttled")

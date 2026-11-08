#!/usr/bin/env python3
"""How long a healthy blob download from a real registry goes without a byte (PM M486).

    silences.py OUT.jsonl ROUNDS CONNECTIONS REF...
    silences.py --summary OUT.jsonl

REF is REGISTRY/REPOSITORY:TAG, anonymous (ghcr.io/actions/actions-runner:latest,
registry-1.docker.io/library/python:3.13). Each REF's manifest (linux/amd64's, from an
index) is asked once, its largest layers listed, and then for ROUNDS rounds the largest
CONNECTIONS layers are downloaded at once, each over a connection of its own, following
the registry's redirect, as a pull's layers are. Every download writes a line: how long
its answer's head took (the redirect's and the blob's), the longest silence between two
reads of up to 64 KiB, how many silences passed 1 s, its bytes (fewer than the layer's
where its answer was cut short) and its time. No layer is
stored. Docker Hub counts the manifest asks as pulls (two a REF): run it sparingly.
"""
import http.client
import json
import ssl
import sys
import threading
import time
import urllib.parse

TIMEOUT = 300
CTX = ssl.create_default_context()
ACCEPT = ", ".join([
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
])


def get(url, headers):
    u = urllib.parse.urlsplit(url)
    conn = http.client.HTTPSConnection(u.hostname, u.port or 443, timeout=TIMEOUT, context=CTX)
    target = u.path + ("?" + u.query if u.query else "")
    conn.request("GET", target, headers={"User-Agent": "shards-probe", **headers})
    return conn, conn.getresponse()


def token(registry, repo):
    conn, r = get(f"https://{registry}/v2/", {})
    challenge = r.getheader("www-authenticate", "")
    r.read()
    conn.close()
    if not challenge.lower().startswith("bearer "):
        return {}
    fields = dict(p.split("=", 1) for p in challenge[7:].replace('"', "").split(","))
    q = urllib.parse.urlencode({"service": fields.get("service", ""), "scope": f"repository:{repo}:pull"})
    conn, r = get(f"{fields['realm']}?{q}", {})
    body = json.loads(r.read())
    conn.close()
    return {"Authorization": "Bearer " + (body.get("token") or body["access_token"])}


def layers(ref):
    registry, rest = ref.split("/", 1)
    repo, tag = rest.rsplit(":", 1)
    auth = token(registry, repo)
    conn, r = get(f"https://{registry}/v2/{repo}/manifests/{tag}", {"Accept": ACCEPT, **auth})
    doc = json.loads(r.read())
    conn.close()
    if "manifests" in doc:
        m = next(m for m in doc["manifests"]
                 if m.get("platform", {}).get("os") == "linux" and m["platform"].get("architecture") == "amd64")
        conn, r = get(f"https://{registry}/v2/{repo}/manifests/{m['digest']}", {"Accept": ACCEPT, **auth})
        doc = json.loads(r.read())
        conn.close()
    ls = sorted(doc["layers"], key=lambda l: -l["size"])
    return registry, repo, auth, ls


def download(registry, repo, auth, layer, out, lock, ref, rnd):
    rec = {"ref": ref, "round": rnd, "digest": layer["digest"], "size": layer["size"]}
    t0 = time.monotonic()
    try:
        conn, r = get(f"https://{registry}/v2/{repo}/blobs/{layer['digest']}", auth)
        rec["head"] = time.monotonic() - t0
        if r.status in (301, 302, 303, 307, 308):
            loc = r.getheader("location")
            r.read()
            conn.close()
            rec["redirect_host"] = urllib.parse.urlsplit(loc).hostname
            t1 = time.monotonic()
            conn, r = get(loc, {})
            rec["cdn_head"] = time.monotonic() - t1
        rec["status"] = r.status
        last = time.monotonic()
        longest, over1, total = 0.0, 0, 0
        while True:
            chunk = r.read1(64 << 10)
            now = time.monotonic()
            if not chunk:
                break
            gap = now - last
            longest = max(longest, gap)
            over1 += gap > 1.0
            total += len(chunk)
            last = now
        conn.close()
        rec.update(bytes=total, longest_silence=longest, silences_over_1s=over1, took=time.monotonic() - t0)
    except (OSError, http.client.HTTPException) as e:
        rec.update(error=type(e).__name__, took=time.monotonic() - t0)
    with lock:
        out.write(json.dumps(rec) + "\n")
        out.flush()


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))] if xs else float("nan")


def summary(path):
    recs = [json.loads(l) for l in open(path)]
    for ref in sorted({r["ref"] for r in recs}):
        rs = [r for r in recs if r["ref"] == ref]
        # A download whose body ended short of its size was cut: its silences up to the
        # cut are counted, its end said.
        ok = [r for r in rs if "error" not in r and r.get("status") == 200]
        cut = [r for r in ok if r["bytes"] != r["size"]]
        print(f"{ref}: {len(rs)} downloads, {len(ok) - len(cut)} whole, {len(cut)} cut short "
              f"{[(r['bytes'], r['size'], round(r['took'])) for r in cut]}, errors "
              f"{[r.get('error') or r.get('status') for r in rs if r not in ok]}, "
              f"{sum(r['bytes'] for r in ok) / 1e9:.2f} GB, "
              f"{sum(r['bytes'] for r in ok) / max(1e-9, sum(r['took'] for r in ok)) / 1e6:.1f} MB/s a download")
        for key, unit in (("longest_silence", 1000), ("head", 1000), ("cdn_head", 1000),
                          ("silences_over_1s", 1), ("took", 1)):
            xs = [r[key] * unit for r in ok if key in r]
            if xs:
                print(f"  {key}: n {len(xs)} p50 {q(xs, .5):.1f} p90 {q(xs, .9):.1f} "
                      f"p99 {q(xs, .99):.1f} max {max(xs):.1f}")


def main():
    if sys.argv[1] == "--summary":
        summary(sys.argv[2])
        return
    path, rounds, conns, refs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4:]
    lock = threading.Lock()
    with open(path, "a") as out:
        for ref in refs:
            registry, repo, auth, ls = layers(ref)
            for rnd in range(rounds):
                ts = [threading.Thread(target=download, args=(registry, repo, auth, l, out, lock, ref, rnd))
                      for l in ls[:conns]]
                for t in ts:
                    t.start()
                for t in ts:
                    t.join()


if __name__ == "__main__":
    main()

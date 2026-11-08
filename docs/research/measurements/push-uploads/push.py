#!/usr/bin/env python3
"""A push's throughput by how many blobs upload at once (PM M488).

    push.py URL REPO --blobs N --size BYTES [--concurrency 1,2,4,8,16] [--rounds 3]
            [--chunk BYTES]

Each round makes N blobs of SIZE random bytes, new to the registry, and uploads them as
shards' pusher does (crates/registry registry.rs `upload`): a HEAD, a POST that begins an
upload, and one PUT of the whole blob with its digest, each worker on one kept-alive
connection, at most C workers at once. With --chunk, each blob goes up in PATCHes of
that many bytes (distribution-spec v1.1.1, "Pushing a blob in chunks"), then a PUT with
no body, as a registry that refuses a whole blob has it sent. Concurrency levels run in turn, their order
alternating round by round. It prints a line per round (MB/s of the whole, wall seconds)
and, per level, the median and each round's rate, then each blob's time to upload (n,
p50, p90, p99, max). Anonymous registries only (a local `registry:2`, ttl.sh): it sends
no credentials.
"""
import argparse
import hashlib
import http.client
import os
import statistics
import sys
import threading
import time
import urllib.parse


def connect(url):
    u = urllib.parse.urlsplit(url)
    if u.scheme == "https":
        return http.client.HTTPSConnection(u.hostname, u.port or 443, timeout=600)
    return http.client.HTTPConnection(u.hostname, u.port or 80, timeout=600)


def upload(conn, base, repo, blob, digest, chunk=0):
    path = urllib.parse.urlsplit(base).path.rstrip("/")
    conn.request("HEAD", f"{path}/v2/{repo}/blobs/{digest}")
    r = conn.getresponse()
    r.read()
    if r.status == 200:
        raise SystemExit(f"{digest}: there already; the round's blobs must be new")
    conn.request("POST", f"{path}/v2/{repo}/blobs/uploads/", body=b"", headers={"Content-Length": "0"})
    r = conn.getresponse()
    r.read()
    if r.status not in (202, 201):
        raise SystemExit(f"POST {r.status} {r.reason}")
    loc = r.getheader("Location")
    at = 0
    while chunk and at < len(blob):
        piece = blob[at : at + chunk]
        u = urllib.parse.urlsplit(urllib.parse.urljoin(base + "/", loc))
        conn.request(
            "PATCH",
            f"{u.path}?{u.query}" if u.query else u.path,
            body=piece,
            headers={
                "Content-Type": "application/octet-stream",
                "Content-Length": str(len(piece)),
                "Content-Range": f"{at}-{at + len(piece) - 1}",
            },
        )
        r = conn.getresponse()
        r.read()
        if r.status != 202:
            raise SystemExit(f"PATCH {r.status} {r.reason}")
        loc = r.getheader("Location")
        at += len(piece)
    if chunk:
        blob = b""
    u = urllib.parse.urlsplit(urllib.parse.urljoin(base + "/", loc))
    query = (u.query + "&" if u.query else "") + "digest=" + urllib.parse.quote(digest)
    conn.request(
        "PUT",
        f"{u.path}?{query}",
        body=blob,
        headers={"Content-Type": "application/octet-stream", "Content-Length": str(len(blob))},
    )
    r = conn.getresponse()
    r.read()
    if r.status not in (200, 201, 204):
        raise SystemExit(f"PUT {r.status} {r.reason}")


def round_of(base, repo, blobs, concurrency, chunk):
    todo = list(range(len(blobs)))
    lock = threading.Lock()
    times = []

    def work():
        conn = connect(base)
        while True:
            with lock:
                if not todo:
                    break
                i = todo.pop(0)
            blob, digest = blobs[i]
            t = time.monotonic()
            upload(conn, base, repo, blob, digest, chunk)
            with lock:
                times.append(time.monotonic() - t)
        conn.close()

    t0 = time.monotonic()
    threads = [threading.Thread(target=work) for _ in range(min(concurrency, len(blobs)))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return time.monotonic() - t0, times


def q(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p * (len(xs) - 1))))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("repo")
    ap.add_argument("--blobs", type=int, required=True)
    ap.add_argument("--size", type=int, required=True)
    ap.add_argument("--concurrency", default="1,2,4,8,16")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--chunk", type=int, default=0)
    a = ap.parse_args()
    levels = [int(c) for c in a.concurrency.split(",")]
    rates = {c: [] for c in levels}
    blob_times = {c: [] for c in levels}
    total = a.blobs * a.size
    for r in range(a.rounds):
        order = levels if r % 2 == 0 else list(reversed(levels))
        for c in order:
            blobs = []
            for _ in range(a.blobs):
                b = os.urandom(a.size)
                blobs.append((b, "sha256:" + hashlib.sha256(b).hexdigest()))
            wall, times = round_of(a.url.rstrip("/"), a.repo, blobs, c, a.chunk)
            rate = total / wall / 1e6
            rates[c].append(rate)
            blob_times[c].extend(times)
            print(f"round {r} concurrency {c}: {rate:.1f} MB/s, {wall:.2f} s", flush=True)
    how = f"in {a.chunk}-byte chunks" if a.chunk else "whole"
    print(f"# {a.blobs} blobs of {a.size} bytes {how}, {a.rounds} rounds, {a.url}")
    for c in levels:
        t = blob_times[c]
        print(
            f"concurrency {c:2d}: median {statistics.median(rates[c]):.1f} MB/s"
            f" ({', '.join(f'{x:.1f}' for x in rates[c])});"
            f" a blob's upload n={len(t)} p50 {q(t, .5)*1e3:.0f} ms p90 {q(t, .9)*1e3:.0f}"
            f" p99 {q(t, .99)*1e3:.0f} max {max(t)*1e3:.0f}"
        )


if __name__ == "__main__":
    sys.exit(main())

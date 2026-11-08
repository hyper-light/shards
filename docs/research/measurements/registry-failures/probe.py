#!/usr/bin/env python3
"""How often real registries fail a request, and for how long (PM M486).

    probe.py OUT.jsonl SECONDS [INTERVAL]

Asks each endpoint below every INTERVAL seconds (default 2), each on a connection of its
own, for SECONDS in all, and writes a line for every answer: the endpoint, the time, the
status or the transport error, and how long it took. An endpoint that fails (a 5xx, a 429,
a timeout, a reset, a refused or failed connection) is asked again every 50 ms until it
answers, so that each failure's length is measured to about that, not to the interval.

Only anonymous requests that pull nothing: Docker Hub's token server (which counts no
pull) and each registry's `/v2/` base (a 401 challenge, or 200, is its answer). No
manifest is asked of Docker Hub: anonymous pulls are counted per address.
"""
import http.client
import json
import socket
import ssl
import sys
import threading
import time

ENDPOINTS = [
    ("auth.docker.io", "/token?service=registry.docker.io&scope=repository:library/alpine:pull"),
    ("registry-1.docker.io", "/v2/"),
    ("ghcr.io", "/token?service=ghcr.io&scope=repository:homebrew/core/wget:pull"),
    ("ghcr.io", "/v2/"),
    ("quay.io", "/v2/"),
    ("public.ecr.aws", "/token/?service=public.ecr.aws&scope=aws"),
    ("gcr.io", "/v2/"),
    ("us-docker.pkg.dev", "/v2/"),
    ("registry.k8s.io", "/v2/"),
    ("mcr.microsoft.com", "/v2/"),
    ("registry.gitlab.com", "/v2/"),
]
TIMEOUT = 30
FAST = 0.05


def ask(host, path):
    t0 = time.monotonic()
    try:
        conn = http.client.HTTPSConnection(host, timeout=TIMEOUT, context=ssl.create_default_context())
        conn.request("GET", path, headers={"User-Agent": "shards-probe"})
        resp = conn.getresponse()
        resp.read(64 << 10)
        conn.close()
        return resp.status, None, time.monotonic() - t0
    except (OSError, http.client.HTTPException, ssl.SSLError) as e:
        kind = type(e).__name__
        if isinstance(e, socket.timeout):
            kind = "timeout"
        return None, kind, time.monotonic() - t0


def failed(status):
    return status is None or status == 429 or status >= 500


def run(host, path, until, interval, out, lock):
    while time.monotonic() < until:
        began = time.monotonic()
        status, error, took = ask(host, path)
        rec = {"host": host, "path": path, "t": time.time(), "status": status, "error": error, "took": took}
        with lock:
            out.write(json.dumps(rec) + "\n")
            out.flush()
        if failed(status):
            # Ridden out at 50 ms, each attempt recorded, to the first answer.
            while time.monotonic() < until:
                time.sleep(FAST)
                status, error, took = ask(host, path)
                rec = {"host": host, "path": path, "t": time.time(), "status": status, "error": error,
                       "took": took, "again": True}
                with lock:
                    out.write(json.dumps(rec) + "\n")
                    out.flush()
                if not failed(status):
                    break
        left = interval - (time.monotonic() - began)
        if left > 0:
            time.sleep(left)


def main():
    path, seconds = sys.argv[1], float(sys.argv[2])
    interval = float(sys.argv[3]) if len(sys.argv) > 3 else 2.0
    until = time.monotonic() + seconds
    lock = threading.Lock()
    with open(path, "a") as out:
        threads = [threading.Thread(target=run, args=(h, p, until, interval, out, lock)) for h, p in ENDPOINTS]
        for t in threads:
            t.start()
        for t in threads:
            t.join()


if __name__ == "__main__":
    main()

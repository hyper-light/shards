#!/usr/bin/env python3
"""M234: what a search's "runs here" lookups cost against Docker Hub (D172).

Docker Hub's index search names repositories; whether each has an image for this
machine's platform, and its size, is a lookup of its own (the Hub API's
/v2/namespaces/NS/repositories/REPO/tags/latest). This asks the index once for TERM,
then looks every result up RUNS times at each concurrency, a pause between rounds,
and prints each request's latency and each set's wall time (n, p50, p90, p99, max),
with every status that was not 200 (a 429 is Hub's abuse limit, which Docker
documents without a number).

    python3 -I measure.py [TERM] [RUNS]
"""

import concurrent.futures
import json
import os
import platform
import subprocess
import sys
import time
import urllib.error
import urllib.request

INDEX = "https://index.docker.io/v1/search"
HUB = "https://hub.docker.com/v2/namespaces/{ns}/repositories/{repo}/tags/latest"
LEVELS = (1, 4, 25)
PAUSE = 5.0


def get(url):
    """The status, and how long it took in ms."""
    start = time.monotonic()
    request = urllib.request.Request(url, headers={"User-Agent": "shards-measure/M234"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            response.read()
            status = response.status
    except urllib.error.HTTPError as e:
        status = e.code
    except OSError as e:
        status = f"error: {e}"
    return status, (time.monotonic() - start) * 1000.0


def names(term, n):
    url = f"{INDEX}?q={urllib.parse.quote(term)}&n={n}"
    request = urllib.request.Request(url, headers={"User-Agent": "shards-measure/M234"})
    with urllib.request.urlopen(request, timeout=60) as response:
        found = json.load(response)["results"]
    out = []
    for r in found:
        ns, _, repo = r["name"].rpartition("/")
        out.append((ns or "library", repo))
    return out


def quantiles(values):
    s = sorted(values)
    pick = lambda q: s[min(len(s) - 1, int(q * len(s)))]
    return f"n={len(s)} p50={pick(0.5):.0f} p90={pick(0.9):.0f} p99={pick(0.99):.0f} max={s[-1]:.0f}"


def main():
    term = sys.argv[1] if len(sys.argv) > 1 else "python"
    runs = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    repos = names(term, 25)
    try:
        revision = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, check=False
        ).stdout.strip()
    except OSError:
        revision = "?"
    print(f"host {platform.node()} {platform.system()} {platform.release()} {platform.machine()}, revision {revision}")
    print(f"term {term!r}: {len(repos)} repositories, {runs} rounds at each of {LEVELS}")
    for level in LEVELS:
        latencies, walls, statuses = [], [], {}
        for _ in range(runs):
            urls = [HUB.format(ns=ns, repo=repo) for ns, repo in repos]
            start = time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=level) as pool:
                for status, ms in pool.map(get, urls):
                    latencies.append(ms)
                    statuses[status] = statuses.get(status, 0) + 1
            walls.append((time.monotonic() - start) * 1000.0)
            time.sleep(PAUSE)
        print(f"at {level:>2}: request ms {quantiles(latencies)}; set ms {quantiles(walls)}; statuses {statuses}")


if __name__ == "__main__":
    os.environ.setdefault("PYTHONIOENCODING", "utf-8")
    main()

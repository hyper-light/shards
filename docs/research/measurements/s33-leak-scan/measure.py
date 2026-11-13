"""Before and after of a scan.py on scan_test.py's runs_image: one scan's time, heap peak and
resident peak, each in a process of its own, and a digest of what it said.

    python3 -I measure.py image PATH SIZE
    python3 -I measure.py scan SCAN_PY PATH time|heap
    python3 -I measure.py prefixes SCAN_PY BYTES

`time` runs the scan as it runs; `heap` under tracemalloc, which slows it, for its traced peak.
The resident peak (getrusage, where the host has it) counts the image's mapped pages too.
`prefixes` times the credential shapes over BYTES of "eyJ" repeated, a run of JWT prefixes.
"""

import hashlib
import importlib.util
import os
import sys
import time
import tracemalloc

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main(argv):
    test = load("scan_test", os.path.join(HERE, "scan_test.py"))
    if argv[0] == "image":
        planted = test.runs_image(argv[1], int(argv[2]))
        print("image %s: %d bytes, %d values planted" % (argv[1], int(argv[2]), len(planted)))
        return 0
    scan = load("scan", argv[1])
    if argv[0] == "prefixes":
        buf = b"eyJ" * (int(argv[2]) // 3)
        index = scan.Index([("secrets.json:accessToken", test.ACCESS)], [])
        began = time.perf_counter()
        found = scan.shapes(buf, index)
        print("%s prefixes %d: %.3f s, %d shapes" % (argv[1], len(buf), time.perf_counter() - began, len(found)))
        return 0
    index = scan.Index([("secrets.json:accessToken", test.ACCESS), ("secrets.json:refreshToken", test.REFRESH)], [])
    said = []
    if argv[3] == "heap":
        tracemalloc.start()
    began = time.perf_counter()
    counts = scan.scan([argv[2]], index, said.append)
    elapsed = time.perf_counter() - began
    peak = tracemalloc.get_traced_memory()[1] if argv[3] == "heap" else None
    try:
        import resource

        # ru_maxrss is in bytes on macOS, KiB on Linux (getrusage(2) of each).
        rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * (1 if sys.platform == "darwin" else 1024)
    except ImportError:
        rss = None
    print(
        "%s %s: %.2f s, heap peak %s, resident peak %s, %d secret runs, said %s"
        % (
            argv[1],
            argv[3],
            elapsed,
            peak,
            rss,
            counts["leaks"],
            hashlib.sha256("\n".join(said).encode()).hexdigest()[:16],
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

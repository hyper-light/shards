"""PM M533: what a new executable's first start costs on macOS, against its second.

Each round writes a new script (`#!/bin/sh`, printing a byte) in a new directory and
starts it, then starts it again; it also starts a new copy of /usr/bin/true, and runs
/bin/sh on a new script it reads (the script itself never started). Prints n, p50, p90,
p99 and max of each, in ms, as subprocess.run times them, start to exit.

    python3 -I first-exec.py BASE_DIR ROUNDS
"""

import os
import shutil
import subprocess
import sys
import tempfile
import time


def run(argv):
    began = time.perf_counter()
    subprocess.run(argv, stdout=subprocess.DEVNULL, check=True)
    return (time.perf_counter() - began) * 1000


def summary(xs):
    xs = sorted(xs)
    at = lambda q: xs[min(len(xs) - 1, int(q * len(xs)))]
    return f"n={len(xs)} p50={at(.5):.1f} p90={at(.9):.1f} p99={at(.99):.1f} max={xs[-1]:.1f} ms"


def main():
    base, rounds = sys.argv[1], int(sys.argv[2])
    os.makedirs(base, exist_ok=True)
    first, again, macho, read = [], [], [], []
    for _ in range(rounds):
        d = tempfile.mkdtemp(dir=base)
        script = os.path.join(d, "s")
        with open(script, "w") as f:
            f.write("#!/bin/sh\nprintf x\n")
        os.chmod(script, 0o700)
        first.append(run([script]))
        again.append(run([script]))
        copy = os.path.join(d, "true")
        shutil.copyfile("/usr/bin/true", copy)
        os.chmod(copy, 0o700)
        macho.append(run([copy]))
        text = os.path.join(d, "t")
        with open(text, "w") as f:
            f.write("printf x\n")
        read.append(run(["/bin/sh", text]))
        shutil.rmtree(d)
    print("a new script, its first start:  ", summary(first))
    print("the same script, started again: ", summary(again))
    print("a new copy of /usr/bin/true:    ", summary(macho))
    print("/bin/sh reading a new script:   ", summary(read))


main()

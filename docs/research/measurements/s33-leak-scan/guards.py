"""Guard removals of scan.py (R10): each named edit made to a copy of s33-leak-scan and
harness-bringup-v2, then all of scan_test.py run on the copy. A removal must fail.

    python3 -I guards.py MEASUREMENTS_DIR NAME...

windows-ranges, first-range and more-refused change what Windows alone runs, and first-data what
Unix alone runs: each fails where it runs.
"""

import os
import shutil
import subprocess
import sys
import tempfile

GUARDS = {
    # Windows: no allocated ranges asked, every hole read (the defect).
    "windows-ranges": (
        '    if os.name == "nt":\n        return allocated(f, size)\n',
        '    if os.name == "nt":\n        return None\n',
    ),
    # Every OS: the whole file always (M561's removal).
    "whole-always": (
        "    found = None if zeros is None else data(f, size)\n",
        "    found = None\n",
    ),
    # Widened ranges that meet left apart.
    "no-join": (
        "        if joined and start <= joined[-1][1]:\n",
        "        if False:\n",
    ),
    # Windows: the first allocated range alone.
    "first-range": (
        "        if done:\n            break\n        at = end\n",
        "        break\n",
    ),
    # Windows: ERROR_MORE_DATA taken for a refusal.
    "more-refused": (
        "        if not done and ctypes.get_last_error() != ERROR_MORE_DATA:\n",
        "        if not done:\n",
    ),
    # Unix: the first SEEK_DATA range alone.
    "first-data": (
        "        found.append((start, end))\n        at = end\n",
        "        found.append((start, end))\n        break\n",
    ),
    # A run's last byte left out where a unit is cut short at what is read's end.
    "cut-unit": (
        "            if hi - end < self.units:\n                end = hi\n",
        "",
    ),
}


def main(argv):
    measurements, names = argv[0], argv[1:]
    failed_all = True
    for name in names:
        old, new = GUARDS[name]
        with tempfile.TemporaryDirectory() as root:
            for part in ("s33-leak-scan", "harness-bringup-v2"):
                shutil.copytree(os.path.join(measurements, part), os.path.join(root, part))
            path = os.path.join(root, "s33-leak-scan", "scan.py")
            with open(path) as f:
                text = f.read()
            if text.count(old) != 1:
                print("guard %s: its edit matches %d times, not once" % (name, text.count(old)))
                failed_all = False
                continue
            with open(path, "w") as f:
                f.write(text.replace(old, new))
            run = subprocess.run(
                [sys.executable, "-I", "scan_test.py"],
                cwd=os.path.join(root, "s33-leak-scan"),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            said = run.stdout.decode(errors="replace").splitlines()
            failing = [l for l in said if l.startswith(("FAIL:", "ERROR:"))]
            summary = [l for l in said if l.startswith(("Ran ", "OK", "FAILED"))]
            print("guard %s: exit %d, %s; %s" % (name, run.returncode, " ".join(summary), "; ".join(failing) or "none failed"))
            failed_all = failed_all and run.returncode != 0
    print("guards: %s" % ("every removal failed" if failed_all else "A REMOVAL SURVIVED or did not apply"))
    return 0 if failed_all else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

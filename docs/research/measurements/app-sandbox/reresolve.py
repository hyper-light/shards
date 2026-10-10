# What a VM in App Sandbox taken over after its template is renamed out of its grant can
# still reach (PM M165): the sandboxed probe (probe.c, `reresolve`) makes a file in its
# granted directory, opens a descriptor of it and plants a symlink there, says `ready`, and
# once this unsandboxed parent has renamed the directory (as run.rs `settle` renames a
# template into place), writes through the descriptor and resolves its bookmark again.
# Under $HOME and under the per-user temporary directory, where a test's SHARDS_HOME is.
#   python3 reresolve.py WORKDIR
import os, subprocess, sys, shutil
work = sys.argv[1]
os.chdir(work)
tmp = subprocess.check_output(["getconf", "DARWIN_USER_TEMP_DIR"]).decode().strip()
for base in [os.path.expanduser("~/.shards-probe-reresolve"), os.path.join(tmp, "shards-probe-reresolve")]:
    shutil.rmtree(base, ignore_errors=True)
    os.makedirs(base)
    d = os.path.join(base, "fresh")
    moved = os.path.join(base, "template")
    os.makedirs(d)
    mark = subprocess.check_output(["./bookmark", d]).decode().strip()
    print(f"under {base}:")
    p = subprocess.Popen(["./probe", "reresolve", f"{mark}@{d}@{moved}"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    for line in p.stdout:
        if line.startswith("HOME") or line.startswith("main_us"):
            continue
        print(line, end="")
        if line.strip() == "ready":
            os.rename(d, moved)
            p.stdin.write("x"); p.stdin.flush()
    p.wait()
    print("  entries at the new path:", sorted(os.listdir(moved)))
    for name in sorted(os.listdir(moved)):
        full = os.path.join(moved, name)
        if os.path.islink(full):
            print(f"  {name} -> {os.readlink(full)}")
    shutil.rmtree(base, ignore_errors=True)

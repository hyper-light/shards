# Whether a directory bookmark's sandbox extension follows the directory when its parent
# renames it (PM M102): the sandboxed probe (probe.c, `renamed`) makes a file in the
# directory, says `ready`, and once this unsandboxed parent has renamed it, tries the new
# path and the old. Run by run.sh, in its work directory.
#   python3 renamed.py WORKDIR
import os, subprocess, sys, shutil
work = sys.argv[1]
os.chdir(work)
home = os.path.expanduser("~/.shards-probe-rename")
shutil.rmtree(home, ignore_errors=True)
os.makedirs(home)
d = os.path.join(home, "fresh")
moved = os.path.join(home, "template")
os.makedirs(d)
mark = subprocess.check_output(["./bookmark", d]).decode().strip()
p = subprocess.Popen(["./probe", "renamed", f"{mark}@{d}@{moved}"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
for line in p.stdout:
    if line.startswith("HOME") or line.startswith("main_us"):
        continue
    print(line, end="")
    if line.strip() == "ready":
        os.rename(d, moved)
        p.stdin.write("x"); p.stdin.flush()
p.wait()
print("files at the new path:", sorted(os.listdir(moved)), "old path exists:", os.path.exists(d))
shutil.rmtree(home, ignore_errors=True)

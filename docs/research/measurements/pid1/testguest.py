"""The containers tests' stop scenarios (PM M145), with the test image's own testguest, under
Docker in shards-dind (D115): what Docker answers where the command is PID 1, and under
--init. Builds w-d115-testguest:1 from a context on stdin; creates only w-d115-* containers
and removes only those, and the image."""
import io
import subprocess
import sys
import tarfile
import time

TESTGUEST = sys.argv[1]
IMG = "w-d115-testguest:1"
DIND = ["docker", "exec", "shards-dind"]
names = []

dockerfile = b"""FROM scratch
COPY testguest /bin/testguest
COPY passwd /etc/passwd
COPY group /etc/group
USER app
ENV FROM_IMAGE=yes PATH=/bin
WORKDIR /work
ENTRYPOINT ["/bin/testguest"]
CMD ["report"]
"""
files = {
    "Dockerfile": (dockerfile, 0o644),
    "testguest": (open(TESTGUEST, "rb").read(), 0o755),
    "passwd": (b"root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:app:/home/app:/bin/sh\n", 0o644),
    "group": (b"root:x:0:\napp:x:1000:\nstaff:x:50:app\n", 0o644),
}
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w") as tar:
    for name, (data, mode) in files.items():
        info = tarfile.TarInfo(name)
        info.size, info.mode = len(data), mode
        tar.addfile(info, io.BytesIO(data))
built = subprocess.run(["docker", "exec", "-i", "shards-dind", "docker", "build", "-q", "-t", IMG, "-"], input=buf.getvalue(), capture_output=True)
print("build", built.returncode, built.stdout.decode().strip(), built.stderr.decode().strip()[-300:], flush=True)


def d(*argv, timeout=120):
    t = time.monotonic()
    p = subprocess.run(DIND + ["docker", *argv], capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout, p.stderr, time.monotonic() - t


def say(label, r):
    code, out, err, secs = r
    print(f"== {label}: exit {code} in {secs:.2f} s\n{out}{err}", flush=True)


def ready(name):
    for _ in range(100):
        if d("logs", name)[1].startswith("ready"):
            return
        time.sleep(0.05)


def started(name, flags, cmd):
    names.append(name)
    say(f"run {name}", d("run", "-d", "--name", name, *flags, IMG, *cmd))
    ready(name)


# events: the containers test's steps, then the actions Docker records.
since = d("run", "--rm", "--entrypoint", "/bin/testguest", IMG, "report")  # warm the image
since = str(int(time.time()) - 1)
started("w-d115-ev", [], ["sleep"])
for step in [["pause", "w-d115-ev"], ["unpause", "w-d115-ev"], ["rename", "w-d115-ev", "w-d115-ev2"]]:
    say(" ".join(step), d(*step))
names.append("w-d115-ev2")
say("stop -t 1 (PID 1, sleep)", d("stop", "-t", "1", "w-d115-ev2"))
say("exit", d("inspect", "-f", "{{.State.ExitCode}}", "w-d115-ev2"))
say("rm", d("rm", "w-d115-ev2"))
until = str(int(time.time()) + 1)
say("events", d("events", "--since", since, "--until", until, "--filter", "type=container", "--filter", "container=w-d115-ev2", "--format", "{{.Action}} {{json .Actor.Attributes}}"))
# The other five: each as PID 1, then under --init.
for flags in [[], ["--init"]]:
    tag = "init" if flags else "pid1"
    started(f"w-d115-sl-{tag}", flags, ["sleep"])
    say(f"stop -t 1 sleep {tag}", d("stop", "-t", "1", f"w-d115-sl-{tag}"))
    say("exit", d("inspect", "-f", "{{.State.ExitCode}}", f"w-d115-sl-{tag}"))
    started(f"w-d115-ss-{tag}", flags + ["--stop-signal", "SIGUSR1"], ["sleep"])
    say(f"stop (default timeout) --stop-signal SIGUSR1 sleep {tag}", d("stop", f"w-d115-ss-{tag}"))
    say("exit", d("inspect", "-f", "{{.State.ExitCode}}", f"w-d115-ss-{tag}"))
    started(f"w-d115-cat-{tag}", flags + ["-i"], ["cat"])
    say(f"stop (default timeout) cat {tag}", d("stop", f"w-d115-cat-{tag}"))
    say("exit", d("inspect", "-f", "{{.State.ExitCode}}", f"w-d115-cat-{tag}"))
# A command that sends itself SIGKILL (testguest kill, then exit 1): as PID 1 and under --init.
for flags in [[], ["--init"]]:
    say(f"run kill {' '.join(flags) or 'pid1'}", d("run", "--rm", *flags, IMG, "kill"))
for n in names:
    d("rm", "-f", n)
d("rmi", IMG)

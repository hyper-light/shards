"""D115's PID 1 cases (PM M145), run through a shards binary, to set beside docker.py's
answers: `shards.py SHARDS HOME [basic] [signals] [s6]`, HOME a new directory each time;
nothing is removed. The runs boot shards' pinned kernel and the init SHARDS carries."""
import os
import subprocess
import sys
import time

BIN, HOME = sys.argv[1], sys.argv[2]
WHICH = sys.argv[3:] or ["all"]
ALPINE = "alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8"
S6 = "ghcr.io/linuxserver/baseimage-alpine:3.22@sha256:ab81abc99e45ef5b045a6c6f41fb2f3ac3651b89f81b0942e1d8861650605cb0"
os.makedirs(HOME, exist_ok=True)
env = dict(os.environ, SHARDS_HOME=HOME)
env.pop("SHARDS_KERNEL", None)
env.pop("SHARDS_INIT", None)


def probe(label, argv, timeout=300):
    t = time.monotonic()
    p = subprocess.run([BIN] + argv, capture_output=True, text=True, timeout=timeout, env=env, cwd=HOME)
    secs = time.monotonic() - t
    print(f"== {label} (exit {p.returncode}, {secs:.3f} s)\n$ shards {' '.join(argv)}\n{p.stdout}{p.stderr}", flush=True)
    return p.returncode, p.stdout, p.stderr, secs


def want(group):
    return "all" in WHICH or group in WHICH


probe("pull alpine", ["pull", "-q", ALPINE])
if want("basic"):
    probe("echo-pid", ["run", ALPINE, "sh", "-c", "echo $$"])
    probe("echo-pid-init", ["run", "--init", ALPINE, "sh", "-c", "echo $$; cat /proc/1/comm; tr '\\0' ' ' </proc/1/cmdline; echo"])
    probe("ps", ["run", ALPINE, "ps", "-o", "pid,ppid,comm"])
    probe("ps-init", ["run", "--init", ALPINE, "ps", "-o", "pid,ppid,comm"])
    probe("kill-self", ["run", ALPINE, "sh", "-c", "kill -TERM 1; kill -KILL 1; echo alive"])
    probe("kill-self-term-init", ["run", "--init", ALPINE, "sh", "-c", "kill -TERM 1; sleep 3; echo alive"])
    probe("kill-self-term-init-user", ["run", "--init", "-u", "1000", ALPINE, "sh", "-c", "kill -TERM 1; sleep 3; echo alive"])
    probe("sysctls", ["run", ALPINE, "sysctl", "net.ipv4.ping_group_range", "net.ipv4.ip_unprivileged_port_start"])
    probe("caps", ["run", ALPINE, "grep", "Cap", "/proc/self/status"])
    probe("ping-without-raw", ["run", "--cap-drop", "NET_RAW", ALPINE, "ping", "-c", "1", "-W", "2", "127.0.0.1"])
    probe("pid-host", ["run", "--pid", "host", ALPINE, "sh", "-c", "echo $$; cat /proc/2/comm; ls /proc/1/root >/dev/null 2>&1 && echo reached || echo denied"])
    probe("pid-invalid", ["run", "--pid", "foo", ALPINE, "true"])
    probe("pid-container-missing", ["run", "--pid", "container:w-d115-nope", ALPINE, "true"])
    probe("create-pid-container-missing", ["create", "--pid", "container:w-d115-nope", ALPINE, "true"])
    for name, flags in [("w-d115-i0", []), ("w-d115-i1", ["--init"]), ("w-d115-i2", ["--init=false"]), ("w-d115-i3", ["--pid", "host"])]:
        probe(f"create {name}", ["create", "--name", name] + flags + [ALPINE, "true"])
        probe(f"inspect {name}", ["inspect", "-f", "{{json .HostConfig.Init}} {{json .HostConfig.PidMode}} {{if .HostConfig.Init}}set{{else}}unset{{end}}", name])
    probe("pid-container-existing", ["run", "--pid", "container:w-d115-i0", ALPINE, "true"])
if want("signals"):
    for name, flags, cmd, t in [
        ("w-d115-sleep", [], ["sleep", "1000"], "2"),
        ("w-d115-sleepinit", ["--init"], ["sleep", "1000"], "2"),
        ("w-d115-trap", [], ["sh", "-c", "trap 'exit 7' TERM; while :; do sleep 0.1; done"], "5"),
    ]:
        probe(f"start {name}", ["run", "-d", "--name", name] + flags + [ALPINE] + cmd)
        time.sleep(1)
        probe(f"top {name}", ["top", name])
        probe(f"stop {name}", ["stop", "-t", t, name])
        probe(f"exit {name}", ["inspect", "-f", "{{.State.ExitCode}} {{.State.OOMKilled}}", name])
    probe("start w-d115-kterm", ["run", "-d", "--name", "w-d115-kterm", ALPINE, "sleep", "1000"])
    probe("kill -s TERM", ["kill", "-s", "TERM", "w-d115-kterm"])
    time.sleep(1)
    probe("running after TERM", ["inspect", "-f", "{{.State.Running}}", "w-d115-kterm"])
    probe("kill", ["kill", "w-d115-kterm"])
    probe("exit after kill", ["inspect", "-f", "{{.State.ExitCode}}", "w-d115-kterm"])
    probe("start w-d115-ex", ["run", "-d", "--name", "w-d115-ex", ALPINE, "sleep", "1000"])
    probe("exec echo-pid", ["exec", "w-d115-ex", "sh", "-c", "echo $$; tr '\\0' ' ' </proc/1/cmdline; echo"])
    probe("exec ps", ["exec", "w-d115-ex", "ps", "-o", "pid,ppid,comm"])
    probe("kill w-d115-ex", ["kill", "w-d115-ex"])
    probe("start w-d115-exi", ["run", "-d", "--init", "--name", "w-d115-exi", ALPINE, "sleep", "1000"])
    probe("exec ps --init", ["exec", "w-d115-exi", "ps", "-o", "pid,ppid,comm"])
    probe("top --init", ["top", "w-d115-exi"])
    probe("kill w-d115-exi", ["kill", "w-d115-exi"])
    probe("start w-d115-rs", ["run", "-d", "--restart", "on-failure:2", "--name", "w-d115-rs", ALPINE, "sh", "-c", "echo $$; exit 3"])
    for _ in range(60):
        code, out, _, _ = probe("restarts", ["inspect", "-f", "{{.RestartCount}} {{.State.ExitCode}} {{.State.Status}}", "w-d115-rs"])
        if out.strip() == "2 3 exited":
            break
        time.sleep(0.5)
    probe("restart logs", ["logs", "w-d115-rs"])
if want("s6"):
    probe("pull s6", ["pull", "-q", S6], timeout=600)
    probe("s6 run", ["run", S6, "echo", "hello from s6"])
    probe("s6 run --init", ["run", "--init", S6, "echo", "hello from s6"])
    probe("s6 run --pid host", ["run", "--pid", "host", S6, "echo", "hello from s6"])
probe("stop daemon", ["stop", "daemon"])

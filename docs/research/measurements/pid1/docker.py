"""D115's Docker ground truth (PM M145), measured in shards-dind: PID 1, --init, --pid,
signals, s6-overlay, default sysctls and capabilities, and the flags shards refuses beside
an Agentfile's domains: `docker.py [S6_IMAGE] [OUT.json]`. Creates only w-d115-*
containers and removes only those, and the s6 image if it pulled it."""
import json
import subprocess
import sys
import time

DIND = ["docker", "exec", "shards-dind"]
ALPINE = "alpine:3.22"
S6 = sys.argv[1] if len(sys.argv) > 1 else "ghcr.io/linuxserver/baseimage-alpine:3.22"
names = []
results = []


def sh(argv, timeout=180):
    t = time.monotonic()
    p = subprocess.run(DIND + argv, capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout, p.stderr, time.monotonic() - t


def probe(label, argv, timeout=180):
    code, out, err, secs = sh(argv, timeout)
    results.append({"label": label, "argv": argv, "exit": code, "stdout": out, "stderr": err, "secs": round(secs, 3)})
    print(f"== {label} (exit {code}, {secs:.3f} s)\n$ docker {' '.join(argv[1:])}\n{out}{err}", flush=True)
    return code, out, err, secs


def named(name):
    names.append(name)
    return name


base = min(sh(["true"])[3] for _ in range(5))
print(f"docker exec baseline: {base:.3f} s", flush=True)
probe("version", ["docker", "version", "--format", "{{.Client.Version}} {{.Server.Version}} {{.Server.Arch}}"])
probe("pull-alpine", ["docker", "pull", "-q", ALPINE])
probe("echo-pid", ["docker", "run", "--rm", ALPINE, "sh", "-c", "echo $$"])
probe("echo-pid-init", ["docker", "run", "--rm", "--init", ALPINE, "sh", "-c", "echo $$"])
probe("ps", ["docker", "run", "--rm", ALPINE, "ps", "-o", "pid,ppid,comm"])
probe("ps-init", ["docker", "run", "--rm", "--init", ALPINE, "ps", "-o", "pid,ppid,comm"])
probe("kill-self-term", ["docker", "run", "--rm", ALPINE, "sh", "-c", "kill -TERM 1; echo alive"])
probe("kill-self-kill", ["docker", "run", "--rm", ALPINE, "sh", "-c", "kill -KILL 1; echo alive"])
probe("kill-self-term-init", ["docker", "run", "--rm", "--init", ALPINE, "sh", "-c", "kill -TERM 1; sleep 1; echo alive"])
probe("sysctls", ["docker", "run", "--rm", ALPINE, "sysctl", "net.ipv4.ping_group_range", "net.ipv4.ip_unprivileged_port_start"])
probe("caps", ["docker", "run", "--rm", ALPINE, "grep", "Cap", "/proc/self/status"])
probe("ping-without-raw", ["docker", "run", "--rm", "--cap-drop", "NET_RAW", ALPINE, "ping", "-c", "1", "-W", "2", "127.0.0.1"])
probe("pid-host", ["docker", "run", "--rm", "--pid", "host", ALPINE, "sh", "-c", "echo $$"])
probe("pid-invalid", ["docker", "run", "--rm", "--pid", "foo", ALPINE, "true"])
probe("pid-container-empty", ["docker", "run", "--rm", "--pid", "container:", ALPINE, "true"])
probe("pid-container-missing-run", ["docker", "run", "--rm", "--pid", "container:w-d115-nope", ALPINE, "true"])
probe("pid-container-missing-create", ["docker", "create", "--name", named("w-d115-pcm"), "--pid", "container:w-d115-nope", ALPINE, "true"])
probe("oom-kill-disable-warning", ["docker", "run", "--rm", "--oom-kill-disable", ALPINE, "true"])
for name, flags in [("w-d115-i0", []), ("w-d115-i1", ["--init"]), ("w-d115-i2", ["--init=false"]), ("w-d115-i3", ["--pid", "host"])]:
    probe(f"inspect {' '.join(flags) or '(none)'}", ["docker", "create", "--name", named(name)] + flags + [ALPINE, "true"])
    probe(f"inspect-fields {name}", ["docker", "inspect", "-f", "{{json .HostConfig.Init}} {{json .HostConfig.PidMode}}", name])
    probe(f"inspect-template-if {name}", ["docker", "inspect", "-f", "{{if .HostConfig.Init}}set{{else}}unset{{end}} {{.HostConfig.Init}}", name])
# PID 1's signals from outside: stop and kill.
for name, flags, cmd, t in [
    ("w-d115-sleep", [], ["sleep", "1000"], "2"),
    ("w-d115-sleepinit", ["--init"], ["sleep", "1000"], "2"),
    ("w-d115-trap", [], ["sh", "-c", "trap 'exit 7' TERM; while :; do sleep 0.1; done"], "5"),
]:
    probe(f"start {name}", ["docker", "run", "-d", "--name", named(name)] + flags + [ALPINE] + cmd)
    time.sleep(1)
    probe(f"top {name}", ["docker", "top", name])
    probe(f"stop {name}", ["docker", "stop", "-t", t, name])
    probe(f"exit {name}", ["docker", "inspect", "-f", "{{.State.ExitCode}} {{.State.OOMKilled}}", name])
probe("start w-d115-kterm", ["docker", "run", "-d", "--name", named("w-d115-kterm"), ALPINE, "sleep", "1000"])
probe("kill -s TERM", ["docker", "kill", "-s", "TERM", "w-d115-kterm"])
time.sleep(1)
probe("running after TERM", ["docker", "inspect", "-f", "{{.State.Running}}", "w-d115-kterm"])
probe("kill", ["docker", "kill", "w-d115-kterm"])
probe("exit after kill", ["docker", "inspect", "-f", "{{.State.ExitCode}}", "w-d115-kterm"])
# Execs join the command's namespace.
probe("start w-d115-ex", ["docker", "run", "-d", "--name", named("w-d115-ex"), ALPINE, "sleep", "1000"])
probe("exec echo-pid", ["docker", "exec", "w-d115-ex", "sh", "-c", "echo $$; tr '\\0' ' ' </proc/1/cmdline; echo"])
probe("exec ps", ["docker", "exec", "w-d115-ex", "ps", "-o", "pid,ppid,comm"])
probe("start w-d115-exi", ["docker", "run", "-d", "--init", "--name", named("w-d115-exi"), ALPINE, "sleep", "1000"])
probe("exec ps --init", ["docker", "exec", "w-d115-exi", "ps", "-o", "pid,ppid,comm"])
probe("top --init", ["docker", "top", "w-d115-exi"])
# A restart policy: PID 1 each time.
probe("start w-d115-rs", ["docker", "run", "-d", "--restart", "on-failure:2", "--name", named("w-d115-rs"), ALPINE, "sh", "-c", "echo $$; exit 3"])
time.sleep(6)
probe("restarts", ["docker", "inspect", "-f", "{{.RestartCount}} {{.State.ExitCode}} {{.State.Status}}", "w-d115-rs"])
probe("restart logs", ["docker", "logs", "w-d115-rs"])
# The flags shards refuses beside an Agentfile's domains, which Docker accepts.
for label, flags in [
    ("privileged", ["--privileged"]),
    ("cap-add SYS_ADMIN", ["--cap-add", "SYS_ADMIN"]),
    ("cap-add NET_ADMIN", ["--cap-add", "NET_ADMIN"]),
    ("systempaths", ["--security-opt", "systempaths=unconfined"]),
    ("sysctl net", ["--sysctl", "net.ipv4.ip_forward=1"]),
    ("user 200000", ["-u", "200000"]),
    ("pid host", ["--pid", "host"]),
]:
    probe(f"accepted {label}", ["docker", "run", "--rm"] + flags + [ALPINE, "true"])
# s6-overlay, which must be PID 1.
had = sh(["docker", "image", "inspect", S6])[0] == 0
probe("pull s6", ["docker", "pull", "-q", S6], timeout=600)
probe("s6 size", ["docker", "image", "inspect", "-f", "{{.Size}} {{.Architecture}} {{index .RepoDigests 0}} {{json .Config.Entrypoint}} {{json .Config.Cmd}}", S6])
probe("s6 run", ["docker", "run", "--rm", S6, "echo", "hello from s6"], timeout=300)
probe("s6 run --init", ["docker", "run", "--rm", "--init", S6, "echo", "hello from s6"], timeout=300)
probe("s6 run --pid host", ["docker", "run", "--rm", "--pid", "host", S6, "echo", "hello from s6"], timeout=300)
for n in names:
    sh(["docker", "rm", "-f", n])
if not had:
    sh(["docker", "rmi", S6])
json.dump({"baseline": base, "results": results}, open(sys.argv[2] if len(sys.argv) > 2 else "probe_pid1.json", "w"), indent=1)

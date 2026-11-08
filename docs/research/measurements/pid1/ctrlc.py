"""Ctrl-C typed at `docker run -it`, without and with --init (PM M145): the terminal's
SIGINT reaches the command's process group, PID 1 there hearing only what it handles.
The host gives `docker exec -it shards-dind docker run -it …` a pseudo-terminal, whose
raw ^C reaches the run's own terminal; --rm containers."""
import os
import pty
import select
import time

for flags in ([], ["--init"]):
    argv = ["docker", "exec", "-it", "shards-dind", "docker", "run", "-it", "--rm", *flags, "alpine:3.22", "sleep", "6"]
    pid, master = pty.fork()
    if pid == 0:
        os.execvp(argv[0], argv)
    start = time.monotonic()
    time.sleep(3)
    os.write(master, b"\x03")
    shown = b""
    while True:
        ready, _, _ = select.select([master], [], [], 10)
        if not ready:
            break
        try:
            chunk = os.read(master, 4096)
        except OSError:
            break
        if not chunk:
            break
        shown += chunk
    _, status = os.waitpid(pid, 0)
    print(
        f"{' '.join(flags) or '(PID 1)'}: exit {os.waitstatus_to_exitcode(status)} after "
        f"{time.monotonic() - start:.1f} s, the terminal showed {shown!r}",
        flush=True,
    )

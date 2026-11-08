"""Runs a command on a pseudo-terminal of ROWSxCOLS and records every byte it writes,
with when (seconds since start): what a user's terminal would receive.

    record.py ROWS COLS OUT.json -- COMMAND...

Writes {"rows", "cols", "chunks": [[t, base64 bytes], ...], "status"}; prints the byte
count. Bytes stay bytes: a chunk may end inside a character.
"""
import base64, fcntl, json, os, pty, select, struct, sys, termios, time

rows, cols, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
cmd = sys.argv[sys.argv.index("--") + 1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ.setdefault("COLORTERM", "truecolor")
    os.execvp(cmd[0], cmd)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
start, chunks = time.time(), []
while True:
    r, _, _ = select.select([fd], [], [], 0.5)
    if fd in r:
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        chunks.append([round(time.time() - start, 4), base64.b64encode(data).decode()])
_, status = os.waitpid(pid, 0)
json.dump({"rows": rows, "cols": cols, "chunks": chunks, "status": os.waitstatus_to_exitcode(status)}, open(out, "w"))
print(sum(len(base64.b64decode(c[1])) for c in chunks), "bytes,", len(chunks), "writes, status", os.waitstatus_to_exitcode(status))

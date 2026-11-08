import socket, subprocess, sys, threading
# Each connection to 127.0.0.1:PORT becomes `docker exec -i shards-dind nc HOST PORT`.
host, port = sys.argv[1], sys.argv[2]
l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
l.bind(("127.0.0.1", 0)); l.listen(64)
print(l.getsockname()[1], flush=True)
def serve(c):
    p = subprocess.Popen(["docker", "exec", "-i", "shards-dind", "nc", host, port], stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
    def up():
        try:
            while (b := c.recv(65536)):
                p.stdin.write(b)
        except OSError: pass
        try: p.stdin.close()
        except OSError: pass
    threading.Thread(target=up, daemon=True).start()
    try:
        while (b := p.stdout.read(65536)):
            c.sendall(b)
    except OSError: pass
    c.close(); p.kill()
while True:
    c, _ = l.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()

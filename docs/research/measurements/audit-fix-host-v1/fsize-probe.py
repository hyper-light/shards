import os, resource, signal, sys, tempfile
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
d = tempfile.mkdtemp(dir=os.environ.get("TMPDIR", "/tmp"))
p = os.path.join(d, "f")
fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
soft, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
resource.setrlimit(resource.RLIMIT_FSIZE, (10, hard))
try:
    n = os.write(fd, b"x" * 20)
    print("first write returned", n)
except OSError as e:
    print("first write error", e.errno, e.strerror)
try:
    n = os.write(fd, b"y" * 5)
    print("second write returned", n)
except OSError as e:
    print("second write error", e.errno, e.strerror)
resource.setrlimit(resource.RLIMIT_FSIZE, (soft, hard))
print("size", os.fstat(fd).st_size, "limits restored", resource.getrlimit(resource.RLIMIT_FSIZE) == (soft, hard))
os.close(fd); os.unlink(p); os.rmdir(d)

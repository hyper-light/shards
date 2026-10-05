#!/usr/bin/env python3
"""Whether a local engine keeps the content of an upload cut off part way locked
(platform-measurements.md M118): an OCI layout of one fresh random blob is sent to
`POST /images/load` up to half its blob, and the connection closed; then the same layout
is loaded whole, and a layout of another fresh blob, each with a deadline, and how each
ended is printed.

usage: probe.py SOCKET [BLOB_MIB] [DEADLINE_S]"""
import hashlib, io, json, os, socket, sys, tarfile, time

def layout(blob: bytes) -> bytes:
    sha = lambda b: "sha256:" + hashlib.sha256(b).hexdigest()
    man = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/x-shards-probe",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": sha(b"{}"), "size": 2},
        "layers": [{"mediaType": "application/octet-stream", "digest": sha(blob), "size": len(blob)}]}).encode()
    idx = json.dumps({"schemaVersion": 2, "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
        "digest": sha(man), "size": len(man),
        "annotations": {"org.opencontainers.image.ref.name": "shards.local/load-lock-probe:x"}}]}).encode()
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=tarfile.USTAR_FORMAT) as t:
        for n, b in [("oci-layout", b'{"imageLayoutVersion":"1.0.0"}'), ("index.json", idx),
                     ("blobs/sha256/" + sha(b"{}")[7:], b"{}"), ("blobs/sha256/" + sha(man)[7:], man),
                     ("blobs/sha256/" + sha(blob)[7:], blob)]:
            i = tarfile.TarInfo(n); i.size = len(b); t.addfile(i, io.BytesIO(b))
    return out.getvalue()

def send(path, body, upto, deadline):
    s = socket.socket(socket.AF_UNIX); s.connect(path); s.settimeout(deadline)
    t = time.monotonic()
    try:
        s.sendall(b"POST /images/load?quiet=1 HTTP/1.1\r\nHost: d\r\nContent-Length: %d\r\n\r\n" % len(body))
        s.sendall(body[:upto])
        if upto < len(body):
            return "cut off"
        line = s.recv(200).split(b"\r\n")[0].decode()
        return f"{line} in {time.monotonic() - t:.1f}s"
    except socket.timeout:
        return f"no answer in {deadline}s"
    except BrokenPipeError:
        # The engine answered and closed before taking it all: what it said.
        try:
            said = s.recv(4096).decode(errors="replace").replace("\r\n", " | ")
        except OSError as e:
            said = str(e)
        return f"closed after {time.monotonic() - t:.1f}s: {said[:400]}"
    finally:
        s.close()

path = sys.argv[1]
mib = int(sys.argv[2]) if len(sys.argv) > 2 else 4
deadline = float(sys.argv[3]) if len(sys.argv) > 3 else 120
blob = os.urandom(mib << 20)
body = layout(blob)
print("first upload :", send(path, body, len(body) // 2, deadline), flush=True)
time.sleep(1)
print("same, whole  :", send(path, body, len(body), deadline), flush=True)
print("another blob :", send(path, layout(os.urandom(mib << 20)), 1 << 62, deadline), flush=True)
print("same, again  :", send(path, body, len(body), deadline), flush=True)

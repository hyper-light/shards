#!/usr/bin/env python3
"""Writes crates/gitsign/testdata/corpus.json: armored OpenPGP signature blocks for the
oracle. Real ones, made by gpg with keys of each algorithm it signs with and by the
signed git objects of a buildx checkout, and crafted ones that take a real one apart:
versions, algorithms, subpackets, headers and armor that go-crypto reads, skips or
refuses.

  scripts/gitsign/corpus.py OUT GNUPG_SIGS_JSON BUILDX_CHECKOUT
"""
import base64, json, struct, subprocess, sys

out, gpg_json, bx = sys.argv[1], sys.argv[2], sys.argv[3]
cases = []

def add(name, text):
    cases.append({"name": name, "armored": text})

def crc24(data):
    crc = 0xB704CE
    for b in data:
        crc ^= b << 16
        for _ in range(8):
            crc <<= 1
            if crc & 0x1000000:
                crc ^= 0x1864CFB
    return crc & 0xFFFFFF

def armor(body, kind="PGP SIGNATURE", headers=(), width=64, crc=True, end=True, crlf=False):
    b64 = base64.b64encode(body).decode()
    lines = [f"-----BEGIN {kind}-----"] + [f"{k}: {v}" for k, v in headers] + [""]
    lines += [b64[i:i + width] for i in range(0, len(b64), width)] or [""]
    if crc:
        lines.append("=" + base64.b64encode(struct.pack(">I", crc24(body))[1:]).decode())
    if end:
        lines.append(f"-----END {kind}-----")
    nl = "\r\n" if crlf else "\n"
    return nl.join(lines) + nl

def unarmor(text):
    lines = [l.strip() for l in text.splitlines()]
    i = lines.index("") + 1
    b64 = ""
    for l in lines[i:]:
        if l.startswith("-----END") or (len(l) == 5 and l.startswith("=")):
            break
        b64 += l
    return base64.b64decode(b64)

def new_header(tag, n):
    if n < 192:
        return bytes([0xC0 | tag, n])
    if n < 8384:
        n -= 192
        return bytes([0xC0 | tag, (n >> 8) + 192, n & 0xFF])
    return bytes([0xC0 | tag, 255]) + struct.pack(">I", n)

def split_packet(raw):
    """A new- or old-format packet's tag and body (lengths not partial)."""
    b = raw[0]
    if b & 0x40:
        n = raw[1]
        if n < 192:
            return b & 0x3F, raw[2:2 + n]
        if n < 224:
            ln = ((n - 192) << 8) + raw[2] + 192
            return b & 0x3F, raw[3:3 + ln]
        ln = struct.unpack(">I", raw[2:6])[0]
        return b & 0x3F, raw[6:6 + ln]
    lt = b & 3
    k = 1 << lt
    ln = int.from_bytes(raw[1:1 + k], "big")
    return (b & 0x3F) >> 2, raw[1 + k:1 + k + ln]

def subpackets(data):
    out = []
    while data:
        n = data[0]
        if n < 192:
            ln, data = n, data[1:]
        elif n < 255:
            ln, data = ((n - 192) << 8) + data[1] + 192, data[2:]
        else:
            ln, data = struct.unpack(">I", data[1:5])[0], data[5:]
        out.append(data[:ln])
        data = data[ln:]
    return out

def sublen(n):
    if n < 192:
        return bytes([n])
    n -= 192
    return bytes([(n >> 8) + 192, n & 0xFF])

def sub(kind, payload, critical=False):
    body = bytes([kind | (0x80 if critical else 0)]) + payload
    return sublen(len(body)) + body

def v4(body):
    """A v4 signature body's parts: head (version..hash), hashed, unhashed, rest."""
    hl = struct.unpack(">H", body[4:6])[0]
    hashed = body[6:6 + hl]
    ul = struct.unpack(">H", body[6 + hl:8 + hl])[0]
    unhashed = body[8 + hl:8 + hl + ul]
    rest = body[8 + hl + ul:]
    return body[:4], hashed, unhashed, rest

def build(head, hashed, unhashed, rest):
    return head + struct.pack(">H", len(hashed)) + hashed + struct.pack(">H", len(unhashed)) + unhashed + rest

# Real signatures.
for name, text in json.load(open(gpg_json)).items():
    add("gpg " + name, text)
for obj in ["v0.37.1", "HEAD"]:
    raw = subprocess.run(["git", "-C", bx, "cat-file", "-p", obj], capture_output=True, text=True, check=True).stdout
    if "-----BEGIN PGP SIGNATURE-----" in raw:
        lines = raw.split("\n")
        start = next(i for i, l in enumerate(lines) if "-----BEGIN PGP SIGNATURE-----" in l)
        sig = []
        for l in lines[start:]:
            l = l[len("gpgsig "):] if l.startswith("gpgsig ") else (l[1:] if l.startswith(" ") else l)
            sig.append(l)
            if "-----END PGP SIGNATURE-----" in l:
                break
        add("git " + obj, "\n".join(sig) + "\n")

base_text = json.load(open(gpg_json))["rsa sha256"]
packet = unarmor(base_text)
tag, body = split_packet(packet)
head, hashed, unhashed, rest = v4(body)

def case(name, b, **kw):
    add(name, armor(new_header(2, len(b)) + b, **kw))

case("rebuilt", build(head, hashed, unhashed, rest))
case("version 3", bytes([3]) + body[1:])
case("version 5", bytes([5]) + body[1:])
case("pubkey algorithm 99", body[:2] + bytes([99]) + body[3:])
case("hash md5", body[:3] + bytes([1]) + body[4:])
case("hash sha1", body[:3] + bytes([2]) + body[4:])
case("hash unknown", body[:3] + bytes([100]) + body[4:])
for cut in [1, 3, 5, 7, len(body) - 3, len(body) - 1]:
    case(f"truncated at {cut}", body[:cut])
subs = subpackets(hashed)
no_time = b"".join(bytes([len(s)]) + s for s in subs if s[0] & 0x7F != 2)
case("no creation time", build(head, no_time, unhashed, rest))
case("unknown critical subpacket", build(head, hashed + sub(100, b"x", True), unhashed, rest))
case("unknown subpacket", build(head, hashed + sub(100, b"x"), unhashed, rest))
case("bad creation time", build(head, hashed + sub(2, b"abc"), unhashed, rest))
case("issuer in unhashed only", build(head, hashed, sub(16, b"\x01\x02\x03\x04\x05\x06\x07\x08"), rest))
case("issuer bad length", build(head, hashed + sub(16, b"\x01\x02"), unhashed, rest))
case("fingerprint v4", build(head, hashed + sub(33, b"\x04" + bytes(range(20))), b"", rest))
case("fingerprint v5", build(head, hashed + sub(33, b"\x05" + bytes(range(32))), b"", rest))
case("fingerprint bad length", build(head, hashed + sub(33, b"\x04" + bytes(range(19))), unhashed, rest))
case("non-exportable", build(head, hashed + sub(4, b"\x00"), unhashed, rest))
case("notation", build(head, hashed + sub(20, b"\x80\x00\x00\x00\x00\x03\x00\x02abcxy"), unhashed, rest))
case("notation bad length", build(head, hashed + sub(20, b"\x80\x00\x00\x00\x00\x03\x00\x09abcxy"), unhashed, rest))
case("embedded wrong type", build(head, hashed + sub(32, body), unhashed, rest))
case("zero length subpacket", build(head, hashed + b"\x00", unhashed, rest))
case("subpacket truncated", build(head, hashed + b"\x10\x10", unhashed, rest))
case("rest garbage", build(head, hashed, unhashed, rest) + b"\x00\x01")
add("old format header", armor(bytes([0x80 | (2 << 2) | 1]) + struct.pack(">H", len(body)) + body))
add("old format indeterminate", armor(bytes([0x80 | (2 << 2) | 3]) + body))
add("partial lengths", armor(bytes([0xC2, 0xE0 | 5]) + body[:32] + new_header(0, len(body) - 32)[1:] + body[32:]))
add("marker first", armor(bytes([0xCA, 3]) + b"PGP" + packet))
add("unknown tag first", armor(bytes([0xC0 | 60, 2]) + b"xx" + packet))
add("critical unknown tag first", armor(bytes([0xC0 | 30, 2]) + b"xx" + packet))
add("trust first", armor(bytes([0xCC, 2]) + b"xx" + packet))
add("two signatures", armor(packet + packet))
add("no msb", armor(b"\x02" + packet))
add("empty", armor(b""))
add("headers", armor(packet, headers=[("Version", "x"), ("Comment", "y")]))
add("crlf", armor(packet, crlf=True))
add("no crc", armor(packet, crc=False))
add("no end", armor(packet, end=False))
add("line of 97", armor(packet, width=97))
add("line of 96", armor(packet, width=96))
add("bad base64", armor(packet).replace(base64.b64encode(packet).decode()[:4], "!!!!", 1))
add("text before", "hello\n" + armor(packet))
add("public key block", armor(packet, kind="PGP PUBLIC KEY BLOCK"))
add("not armored", base64.b64encode(packet).decode())
# SSH signatures taken apart.
def ssh_string(b):
    return struct.pack(">I", len(b)) + b

def ssh_fields(b, n):
    out = []
    for _ in range(n):
        ln = struct.unpack(">I", b[:4])[0]
        out.append(b[4:4 + ln])
        b = b[4 + ln:]
    return out, b

def ssh_armor(blob):
    b64 = base64.b64encode(blob).decode()
    return "-----BEGIN SSH SIGNATURE-----\n" + "\n".join(b64[i:i + 70] for i in range(0, len(b64), 70)) + "\n-----END SSH SIGNATURE-----\n"

real = json.load(open(gpg_json))["ssh ed25519"]
blob = base64.b64decode("".join(l for l in real.splitlines() if not l.startswith("-----")))
magic, rest = blob[:6], blob[6:]
version = struct.unpack(">I", rest[:4])[0]
(pub, ns, reserved, hashalg, sig), tail = ssh_fields(rest[4:], 5)

def ssh_case(name, magic=magic, version=version, pub=pub, ns=ns, hashalg=hashalg, sig=sig, tail=tail):
    add(name, ssh_armor(magic + struct.pack(">I", version) + ssh_string(pub) + ssh_string(ns) + ssh_string(reserved) + ssh_string(hashalg) + ssh_string(sig) + tail))

ssh_case("ssh rebuilt")
ssh_case("ssh bad magic", magic=b"SSHSIX")
ssh_case("ssh version 2", version=2)
ssh_case("ssh hash sha1", hashalg=b"sha1")
ssh_case("ssh trailing", tail=b"x")
ssh_case("ssh short key", pub=ssh_string(b"ssh-ed25519") + ssh_string(b"\x00" * 31))
ssh_case("ssh key junk", pub=pub + b"x")
ssh_case("ssh unknown key", pub=ssh_string(b"ssh-foo") + ssh_string(b"\x00" * 32))
ssh_case("ssh rsa-sha2 as key", pub=ssh_string(b"rsa-sha2-256") + ssh_string(b"\x00" * 32))
ssh_case("ssh namespace file", ns=b"file")
ssh_case("ssh truncated", tail=b"", sig=b"")
rsa = json.load(open(gpg_json))["ssh rsa 2048"]
rblob = base64.b64decode("".join(l for l in rsa.splitlines() if not l.startswith("-----")))
(rpub, rns, rres, rhash, rsig), rtail = ssh_fields(rblob[10:], 5)
fmt, rsig_rest = ssh_fields(rsig, 1)
ssh_case("ssh rsa format ssh-rsa", pub=rpub, sig=ssh_string(b"ssh-rsa") + rsig_rest)
e, n = ssh_fields(rpub[4 + 7:], 2)[0]
ssh_case("ssh rsa padded modulus", pub=ssh_string(b"ssh-rsa") + ssh_string(e) + ssh_string(b"\x00" + n), sig=rsig)
ssh_case("ssh rsa even exponent", pub=ssh_string(b"ssh-rsa") + ssh_string(b"\x04") + ssh_string(n), sig=rsig)
ec = json.load(open(gpg_json))["ssh ecdsa 256"]
eblob = base64.b64decode("".join(l for l in ec.splitlines() if not l.startswith("-----")))
(epub, ens, eres, ehash, esig), etail = ssh_fields(eblob[10:], 5)
(etype, ecurve, epoint), _ = ssh_fields(epub, 3)
ssh_case("ssh ecdsa off curve", pub=ssh_string(etype) + ssh_string(ecurve) + ssh_string(epoint[:-1] + bytes([epoint[-1] ^ 1])), sig=esig)
ssh_case("ssh ecdsa compressed", pub=ssh_string(etype) + ssh_string(ecurve) + ssh_string(b"\x02" + epoint[1:33]), sig=esig)
ssh_case("ssh ecdsa curve mismatch", pub=ssh_string(etype) + ssh_string(b"nistp384") + ssh_string(epoint), sig=esig)
add("ssh pem other type", ssh_armor(blob).replace("SSH SIGNATURE-----\n", "SSH SIGNATURE-----\n", 1).replace("-----END SSH SIGNATURE-----", "-----END SSH SIGNATUREX-----"))
json.dump(cases, open(out, "w"), indent=1)

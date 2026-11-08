#!/usr/bin/env python3
"""Builds the verification cases of crates/gitsign/testdata/verify.json: real keys and
signatures made by gpg and ssh-keygen in a throwaway GnuPG home, each case a signature,
the data it is checked against, and the keys it is checked with, as buildx's
verify_git_signature and verify_http_pgp_signature take them. oracle_test.go adds keys
gpg does not make (v6, Ed448) and writes what go-crypto answers to each.

    verify.py OUT WORKDIR
"""

import atexit
import base64
import json
import os
import shutil
import subprocess
import sys
import tempfile

out, work = sys.argv[1], sys.argv[2]
# A short home: gpg-agent's socket path has a length limit. Its agent ends with it.
home = tempfile.mkdtemp(prefix="gitsign-")
env = dict(os.environ, GNUPGHOME=home, TZ="UTC")


def cleanup():
    subprocess.run(["gpgconf", "--kill", "all"], env=env, capture_output=True)
    shutil.rmtree(home, ignore_errors=True)


atexit.register(cleanup)
data = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor a <a@shards.invalid> 1700000000 +0000\n\nsigned\n"
other = data + b"changed\n"
cases = []


def gpg(*args, stdin=None):
    return subprocess.run(
        ["gpg", "--batch", "--quiet", "--yes", "--passphrase", "", "--pinentry-mode", "loopback", *args],
        env=env, input=stdin, capture_output=True, check=True,
    ).stdout


def key(uid, algo, usage="sign", expire="never", at=None):
    faked = ["--faked-system-time", at] if at else []
    gpg(*faked, "--quick-gen-key", uid, algo, usage, expire)
    return fpr(uid)


def fpr(uid):
    out = gpg("--with-colons", "--list-keys", uid).decode()
    return [l.split(":")[9] for l in out.splitlines() if l.startswith("fpr:")][0]


def export(*who, secret=False):
    return gpg("--armor", "--export-secret-keys" if secret else "--export", *who).decode()


def sign(who, payload=data, digest="SHA256", extra=(), at=None):
    faked = ["--faked-system-time", at] if at else []
    return gpg(*faked, "--local-user", who, "--digest-algo", digest, *extra, "--armor", "--detach-sign",
               stdin=payload).decode()


def case(name, kind, signature, keys, payload=data, **more):
    cases.append(dict(name=name, kind=kind, signature=signature, data=base64.b64encode(payload).decode(),
                      keys=keys, **more))


# A key of each algorithm gpg makes, each signing; each signature checked as a Git
# object's and over its digest.
algos = [
    ("rsa2048", "SHA256"), ("rsa3072", "SHA512"), ("rsa4096", "SHA384"), ("rsa1024", "SHA256"),
    ("ed25519", "SHA256"), ("ed25519", "SHA512"), ("nistp256", "SHA256"), ("nistp384", "SHA384"),
    ("nistp521", "SHA512"), ("nistp256", "SHA512"), ("nistp384", "SHA256"), ("nistp521", "SHA224"),
    ("brainpoolP256r1", "SHA256"), ("brainpoolP384r1", "SHA384"), ("brainpoolP512r1", "SHA512"),
    ("secp256k1", "SHA256"), ("dsa2048", "SHA256"), ("dsa3072", "SHA384"), ("rsa2048", "SHA1"),
    ("rsa2048", "SHA224"), ("ed25519", "SHA1"), ("ed448", "SHA512"),
]
made = {}
for algo, digest in algos:
    uid = f"shards-{algo} <{algo}@shards.invalid>"
    if algo not in made:
        try:
            made[algo] = key(uid, algo)
        except subprocess.CalledProcessError as e:
            print(f"gpg makes no {algo} key: {e.stderr.decode().strip()}", file=sys.stderr)
            continue
    try:
        sig = sign(made[algo], digest=digest)
    except subprocess.CalledProcessError as e:
        print(f"gpg signs no {digest} with {algo}: {e.stderr.decode().strip()}", file=sys.stderr)
        continue
    keys = export(made[algo])
    case(f"{algo} {digest}", "git", sig, keys)
    case(f"{algo} {digest} digest", "digest", sig, keys)
    case(f"{algo} {digest} other data", "git", sig, keys, payload=other)
    case(f"{algo} {digest} digest other data", "digest", sig, keys, payload=other)

rsa, ed = made["rsa2048"], made["ed25519"]
good = sign(ed)
case("unknown key", "git", good, export(rsa))
case("both keys", "git", good, export(rsa, ed))
case("two blocks", "git", good, export(rsa) + export(ed))
case("two blocks, signer first", "git", good, export(ed) + export(rsa))
case("two blocks with text between", "git", good, export(rsa) + "\nsome text\n\n" + export(ed))
case("two blocks, no newline between", "git", good, export(rsa).rstrip("\n") + export(ed))
case("private key block", "git", good, export(ed, secret=True))
case("private rsa key block", "git", sign(rsa), export(rsa, secret=True))
case("no keys", "git", good, "")
case("not armor", "git", good, "ssh-ed25519 AAAA")
case("signature block as keys", "git", good, good)
case("text signature", "git", sign(ed, extra=["--textmode"]), export(ed))
case("text signature, CRLF data", "git", sign(ed, payload=data.replace(b"\n", b"\r\n"), extra=["--textmode"]),
     export(ed))
case("critical notation", "git", sign(ed, extra=["--sig-notation", "!x@shards.invalid=1"]), export(ed))
case("notation", "git", sign(ed, extra=["--sig-notation", "x@shards.invalid=1"]), export(ed))
case("signature expiring", "git", sign(ed, extra=["--default-sig-expire", "1d"]), export(ed))

# A signing subkey under a certify-only primary key.
sub = key("shards-subkey <subkey@shards.invalid>", "ed25519", usage="cert")
gpg("--quick-add-key", sub, "nistp256", "sign", "never")
case("subkey", "git", sign(sub), export(sub))
case("subkey digest", "digest", sign(sub), export(sub))
case("subkey, private block", "git", sign(sub), export(sub, secret=True))

# Revoked: the primary key, by its revocation certificate; a user ID.
rev = key("shards-revoked <revoked@shards.invalid>", "ed25519")
before = sign(rev)
cert = open(os.path.join(home, "openpgp-revocs.d", rev + ".rev")).read().replace(":-----BEGIN", "-----BEGIN")
gpg("--import", stdin=cert.encode())
case("revoked key", "git", before, export(rev))
case("revoked key digest", "digest", before, export(rev))
uids = key("shards-uids <uids@shards.invalid>", "ed25519")
gpg("--quick-add-uid", uids, "shards-second <second@shards.invalid>")
gpg("--quick-revoke-uid", uids, "shards-second <second@shards.invalid>")
case("revoked user ID", "git", sign(uids), export(uids))

# Expiry, at the signature's time (BuildKit checks there): a key that expired after it
# signed. (One that had expired when it signed, which gpg will not make, oracle_test.go
# makes.)
exp = key("shards-expired <expired@shards.invalid>", "ed25519", expire="1d", at="20200101T000000")
case("key expired since it signed", "git", sign(exp, at="20200101T010000"), export(exp))

# SSH signatures, as git's gpg.format=ssh makes them.
def ssh_key(name, *args):
    path = os.path.join(work, "id_" + name)
    if not os.path.exists(path):
        subprocess.run(["ssh-keygen", "-q", *args, "-N", "", "-C", f"{name}@shards.invalid", "-f", path], check=True)
    return path, open(path + ".pub").read()


def ssh_sign(path, namespace="git", payload=data, extra=()):
    src = os.path.join(work, "ssh-data")
    with open(src, "wb") as f:
        f.write(payload)
    subprocess.run(["ssh-keygen", "-q", "-Y", "sign", "-n", namespace, *extra, "-f", path, src], check=True,
                   capture_output=True)
    sig = open(src + ".sig").read()
    os.remove(src + ".sig")
    return sig


for name, args in [("ed25519", ["-t", "ed25519"]), ("ecdsa256", ["-t", "ecdsa", "-b", "256"]),
                   ("ecdsa384", ["-t", "ecdsa", "-b", "384"]), ("ecdsa521", ["-t", "ecdsa", "-b", "521"]),
                   ("rsa2048", ["-t", "rsa", "-b", "2048"]), ("rsa1024", ["-t", "rsa", "-b", "1024"])]:
    try:
        path, pub = ssh_key(name, *args)
    except subprocess.CalledProcessError:
        print(f"ssh-keygen makes no {name} key", file=sys.stderr)
        continue
    case(f"ssh {name}", "git", ssh_sign(path), pub)
    case(f"ssh {name} sha256", "git", ssh_sign(path, extra=["-O", "hashalg=sha256"]), pub)
    case(f"ssh {name} other data", "git", ssh_sign(path), pub, payload=other)

path, pub = ssh_key("ed25519", "-t", "ed25519")
_, other_pub = ssh_key("ecdsa256", "-t", "ecdsa", "-b", "256")
sig = ssh_sign(path)
blob = pub.split()[1]
case("ssh namespace file", "git", ssh_sign(path, namespace="file"), pub)
case("ssh other key", "git", sig, other_pub)
case("ssh options", "git", sig, f'restrict,command="echo a b" {pub}')
case("ssh options, quoted comma", "git", sig, f'from="a,b",no-pty {pub}')
case("ssh comments and blank lines", "git", sig, f"# keys\n\n{other_pub}{pub}")
case("ssh type mismatch", "git", sig, f"ssh-rsa {blob} x\n")
case("ssh type mismatch, then the key", "git", sig, f"ssh-rsa {blob} x\n{pub}")
case("ssh no key", "git", sig, "# nothing\n")
case("ssh bad base64", "git", sig, "ssh-ed25519 AAAA*AAA x\n")
case("ssh CRLF", "git", sig, pub.replace("\n", "\r\n"))
case("ssh tab", "git", sig, pub.replace(" ", "\t", 1))
case("not signed", "git", "", pub)
case("not a signature", "git", "hello", pub)

json.dump(cases, open(out, "w"), indent=1)
print(f"{len(cases)} cases", file=sys.stderr)

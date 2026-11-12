#!/usr/bin/env python3
"""Mutations of aes256-only-v1 (R09/R10), each by exact text replacement that must match once.

  base-remote, base-server     the providers of the base (032b63b1c): the red baseline
  chacha-remote, chacha-server ChaCha20-Poly1305 allowed again, after AES-256-GCM
  aes128-remote, aes128-server AES-128-GCM allowed, after AES-256-GCM

  mutate.py NAME          apply mutation NAME, print the file it changed
  mutate.py --file NAME   print the file NAME changes, change nothing

Run from the repository root; mutations.sh applies one, runs the cipher tests and restores it.
"""
import pathlib
import sys

TLS = "crates/remote/src/tls.rs"
SERVER = "crates/server/src/server.rs"
SUITES = "rustls::crypto::aws_lc_rs::cipher_suite::"
NARROWED = f"cipher_suites: vec![{SUITES}TLS13_AES_256_GCM_SHA384],"


def also(suite):
    return f"cipher_suites: vec![{SUITES}TLS13_AES_256_GCM_SHA384, {SUITES}{suite}],"


MUTATIONS = {
    "base-remote": (TLS, [("    &PROVIDER\n", "    hyper_quic::crypto::rustls::node_provider()\n")]),
    "chacha-remote": (TLS, [(NARROWED, also("TLS13_CHACHA20_POLY1305_SHA256"))]),
    "aes128-remote": (TLS, [(NARROWED, also("TLS13_AES_128_GCM_SHA256"))]),
    "base-server": (
        SERVER,
        [(
            "    let provider = Arc::new(rustls::crypto::CryptoProvider {\n"
            f"        {NARROWED}\n"
            "        ..rustls::crypto::aws_lc_rs::default_provider()\n"
            "    });\n",
            "    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());\n",
        )],
    ),
    "chacha-server": (SERVER, [(NARROWED, also("TLS13_CHACHA20_POLY1305_SHA256"))]),
    "aes128-server": (SERVER, [(NARROWED, also("TLS13_AES_128_GCM_SHA256"))]),
}


def main(argv):
    if len(argv) == 3 and argv[1] == "--file" and argv[2] in MUTATIONS:
        print(MUTATIONS[argv[2]][0])
        return 0
    if len(argv) != 2 or argv[1] not in MUTATIONS:
        print(__doc__, file=sys.stderr)
        return 2
    path, edits = MUTATIONS[argv[1]]
    file = pathlib.Path(path)
    text = file.read_text()
    for old, new in edits:
        if text.count(old) != 1:
            print(f"{argv[1]}: {old!r} occurs {text.count(old)} times in {path}", file=sys.stderr)
            return 1
        text = text.replace(old, new)
    file.write_text(text)
    print(path)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

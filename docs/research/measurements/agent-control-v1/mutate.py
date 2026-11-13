#!/usr/bin/env python3
"""Guard-removal mutations for plan step 11's agent-control endpoint (R10): each removes one
guard, runs the test that holds it, and expects that test to fail; the file is restored by
rewriting it (a fresh mtime, so cargo rebuilds the original), and the test run again green.

    python3 -I mutate.py SRC_ROOT [NAME_PREFIX]

Run from a checkout of the branch; build output where CARGO_TARGET_DIR says. Prints per
mutation its name, whether its test ran and failed with the guard gone (killed: a build that
fails kills nothing) and whether it passed again restored, then each run's time, outcome and
failure lines.
"""
import os
import subprocess
import sys
import time

CONTROL = "crates/remote/src/control.rs"
TLS = "crates/remote/src/tls.rs"

# (name, file, guard as written, guard removed, test filter)
MUTATIONS = [
    (
        "the endpoint takes only its enrolled controller's key",
        TLS,
        ".is_none_or(|owner| identity::fingerprint(end_entity) == owner)",
        ".is_none_or(|_| true)",
        "control::tests::a_controller_not_enrolled_is_refused_at_its_handshake",
    ),
    (
        "the controller takes only the microVM's pinned key",
        TLS,
        "if identity::is_ed25519(end_entity) && identity::fingerprint(end_entity) == self.0 {",
        "if identity::is_ed25519(end_entity) {",
        "control::tests::an_endpoint_not_the_pinned_vm_is_refused_at_its_handshake",
    ),
    (
        "the control connection is its own protocol",
        CONTROL,
        "pub const ALPN: &[u8] = b\"shards-control/1\";",
        "pub const ALPN: &[u8] = tls::ALPN;",
        "control::tests::a_peer_of_another_protocol_is_refused",
    ),
    (
        "access_denied reads as a key not enrolled",
        CONTROL,
        "if code == alert(AlertDescription::AccessDenied) {",
        "if code == alert(AlertDescription::BadCertificate) {",
        "control::tests::a_controller_not_enrolled_is_refused_at_its_handshake",
    ),
    (
        "a lane's first frame is a subscription's length",
        CONTROL,
        "if tag != Tag::Exec || len != SUBSCRIPTION {",
        "if tag != Tag::Exec {",
        "control::tests::a_lane_of_no_kind_is_refused",
    ),
    (
        "a lane is of a member's stdin, stdout or stderr",
        CONTROL,
        "        kind::STDERR => Stdio::Stderr,\n        _ => return Err(Reason::Protocol),",
        "        _ => Stdio::Stderr,",
        "control::tests::a_lane_of_no_kind_is_refused",
    ),
]


def test(root, name_filter):
    """Runs the test alone: whether it passed, and what shows why, its outcome line and the
    first lines of its failure, with how long the build and run took."""
    began = time.monotonic()
    run = subprocess.run(
        ["cargo", "test", "-p", "shards-remote", "--lib", name_filter, "--", "--exact"],
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    took = time.monotonic() - began
    lines = run.stdout.splitlines()
    shown = [l for l in lines if l.startswith(("running ", "test result:", "error"))]
    shown += [l for l in lines if "panicked at" in l or l.lstrip().startswith(("left:", "right:"))][:4]
    return run.returncode == 0, f"{took:.1f}s | " + " | ".join(shown)


def main():
    root = sys.argv[1]
    prefix = sys.argv[2] if len(sys.argv) > 2 else ""
    failed = False
    for name, path, guard, removed, name_filter in MUTATIONS:
        if not name.startswith(prefix):
            continue
        full = os.path.join(root, path)
        with open(full, encoding="utf-8") as f:
            original = f.read()
        if original.count(guard) != 1:
            print(f"{name}: guard not found once in {path}")
            failed = True
            continue
        with open(full, "w", encoding="utf-8") as f:
            f.write(original.replace(guard, removed))
        try:
            passed, out = test(root, name_filter)
        finally:
            with open(full, "w", encoding="utf-8") as f:
                f.write(original)
        restored, again = test(root, name_filter)
        # Killed only by its test failing as it runs, never by a build that failed.
        killed = not passed and "running 1 test" in out and "1 failed" in out
        print(f"{name}: killed={killed} restored_green={restored}")
        print(f"    mutant:   {out}")
        print(f"    restored: {again}", flush=True)
        failed |= not (killed and restored)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()

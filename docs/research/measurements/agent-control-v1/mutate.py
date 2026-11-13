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
MANAGED = "crates/remote/src/managed.rs"
INPUT = "crates/remote/src/managed/input.rs"

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
    # slice 2 (c9630c6b4)
    (
        "the controller's newer connection closes the older, Stale",
        CONTROL,
        'while let Some(old) = self.conns.pop() {',
        'while let Some(old) = None::<ConnKey> {',
        'control::tests::lanes::the_controllers_newer_connection_supersedes_the_older_and_resumes_at_its_cursor',
    ),
    # slice 2 (c9630c6b4)
    (
        'an output holds one lane',
        MANAGED,
        '        if !matches!(self.state, Subscription::Listening) {\n            return Err(Reason::Scope);\n        }\n        self.check_scope(scope)?;',
        '        self.check_scope(scope)?;',
        'control::tests::lanes::a_second_lane_of_a_stream_is_refused_and_the_first_goes_on',
    ),
    # slice 2 (c9630c6b4)
    (
        'a stdin holds one lane',
        INPUT,
        '        if !matches!(self.state, Subscription::Listening) {\n            return Err(Reason::Scope);\n        }\n        if self.revoked() {',
        '        if self.revoked() {',
        'control::tests::lanes::a_second_lane_of_a_stream_is_refused_and_the_first_goes_on',
    ),
    # slice 2 (c9630c6b4)
    (
        'a lane not served is refused alone, not its connection',
        CONTROL,
        '                Err(reason) => self.refuse_lane(io, conn, stream, reason),\n            },',
        '                Err(reason) => self.refuse_connection(io, conn, reason),\n            },',
        'control::tests::lanes::a_lane_the_endpoint_does_not_serve_is_refused_alone_and_the_others_go_on',
    ),
    # slice 2 (c9630c6b4)
    (
        'a revoked output ends its lane alone, not the driver',
        CONTROL,
        'outputs.push(output.handles(sender.try_clone()?, Stop::Lane)?);',
        'outputs.push(output.handles(sender.try_clone()?, Stop::Driver)?);',
        'control::tests::lanes::a_revoked_stream_ends_its_lane_alone',
    ),
    # slice 2 (c9630c6b4)
    (
        "a frame refused after its input's end only stops the lane's reading",
        CONTROL,
        'Ok(()) | Err(Gone::Refused(_)) if over => After::Over,',
        'Ok(()) if over => After::Over,',
        'control::tests::a_lanes_reading_ends_it_alone_unless_its_controller_broke_its_protocol',
    ),
    # slice 2 (c9630c6b4)
    (
        "the connection's window is its lanes' summed",
        CONTROL,
        'windows.connection = windows.connection.checked_add(receive).ok_or_else(window)?;',
        'windows.connection = windows.connection.max(receive);',
        'control::tests::lanes::its_windows_are_its_streams_own_summed',
    ),
    # slice 2 (c9630c6b4)
    (
        'a record beside a queued wake keeps its place',
        MANAGED,
        'if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) =\n            self.sender.try_send(Command::Handler(()))',
        'if let Err(_) =\n            self.sender.try_send(Command::Handler(()))',
        'managed::tests::a_record_submitted_beside_a_queued_wake_is_kept_for_its_turn',
    ),
    # slice 2 (c9630c6b4)
    (
        "a finished output's lane is stopped both ways (UNCERTAIN: its test may not hold it)",
        CONTROL,
        '                output.disconnect();\n                io.stop(conn, stream);\n            }\n        }\n        for input in &mut self.inputs {',
        '                output.disconnect();\n            }\n        }\n        for input in &mut self.inputs {',
        'control::tests::lanes::a_controllers_lanes_carry_a_members_stdio_exactly_and_end_as_it_ends',
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

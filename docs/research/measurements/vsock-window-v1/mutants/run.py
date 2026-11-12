"""Guard-removal mutants of vsock-window-v1: each takes one guard out of a copy of the tree,
runs the tests, and must turn the tests named for it red. The file is restored by writing it
again, so its mtime moves and cargo rebuilds (a file moved back keeps an old mtime and cargo
keeps the mutant's build).

Usage: run.py REPO SCRATCH. REPO is a checkout of p/vsock-window-v1; SCRATCH, removed first,
holds the copy and its cargo target.
"""

import re
import shutil
import subprocess
import sys
from pathlib import Path

CONN = "crates/vmm/src/devices/virtio/vsock/conn.rs"
MUXER = "crates/vmm/src/devices/virtio/vsock/muxer.rs"
ENGINE = "crates/init/src/engine.rs"
VMM = ["cargo", "test", "-j4", "-p", "shards-vmm", "--lib", "--", "devices::virtio::vsock"]
INIT = ["cargo", "test", "-j4", "-p", "shards-init", "--target", "x86_64-unknown-linux-gnu", "--", "engine::tests"]
SHARE = "muxer::tests::two_connections_share_the_lent_window"
BUSY = "muxer::tests::a_busy_device_keeps_its_window_bounds"
SOCKETS = "muxer::tests::host_sockets_ask_for_their_buffers_where_this_process_may"
RING = "conn::tests::a_ring_keeps_the_guests_order_and_gives_back_its_room"

# (what the mutant does, file, guard, its replacement, tests that must fail)
MUTANTS = [
    ("M1 a window takes what other connections hold: conn.rs header() without .min(room)",
     CONN, "            .min(room)\n", "", [SHARE, BUSY]),
    ("M2 spare counts nothing others hold: muxer.rs next_rx()",
     MUXER, "let spare = u64::from(WINDOW_LENT).saturating_sub(others);",
     "let spare = u64::from(WINDOW_LENT);", [SHARE, BUSY]),
    ("M3 the right edge may move left: header() without .max(self.exposure())",
     CONN, "            .max(WINDOW_LEAST)\n            .max(self.exposure());",
     "            .max(WINDOW_LEAST);", ["conn::tests::a_window_never_takes_back_what_it_gave", BUSY]),
    ("M4 a window past the guest's buffer: header() without .min(self.guest_buf_alloc)",
     CONN, "            .min(self.guest_buf_alloc)\n", "",
     ["conn::tests::a_window_grows_no_further_than_the_guests_buffer"]),
    ("M5 a window grows without the guest using it: asked is always WINDOW_MOST",
     CONN, "        } else {\n            self.window\n        };", "        } else {\n            WINDOW_MOST\n        };",
     ["conn::tests::a_window_grows_only_once_its_guest_uses_it",
      "muxer::tests::a_lone_connection_is_offered_the_most_window_and_reset_past_it"]),
    ("M6 an update owed only after data: on_guest_packet()'s check for RW alone",
     CONN, "if !self.killed && !self.guest_send_closed && self.guest_needs_credit() {",
     "if h.op == op::RW && !self.killed && !self.guest_send_closed && self.guest_needs_credit() {",
     ["conn::tests::a_guest_whose_buffer_shrinks_is_owed_its_update"]),
    ("M7 a guest may send the most window whatever it was given: send_host()'s check",
     CONN, "if self.tx_buf.len() + total > self.exposure() as usize {",
     "if self.tx_buf.len() + total > WINDOW_MOST as usize {",
     ["conn::tests::a_window_never_takes_back_what_it_gave"]),
    ("M8 lent not recounted as the host takes bytes: muxer.rs on_events()",
     MUXER, "                    self.lent = self.lent.saturating_sub(held).saturating_add(u64::from(c.lent()));\n",
     "", [BUSY]),
    ("M9 buffer room past what the guest may send: append() without .min(limit.max(needed))",
     CONN, "                .max(self.data.capacity().saturating_mul(2))\n                .min(limit.max(needed));",
     "                .max(self.data.capacity().saturating_mul(2));",
     ["conn::tests::a_buffer_never_holds_room_past_the_credit"]),
    ("M10 room never given back: fit() always returns",
     CONN, "        if room <= WINDOW_LEAST as usize || room <= limit.saturating_mul(2) {",
     "        if room > 0 {", [RING]),
    ("M11 new bytes that span the ring's wrap written second part first: append()",
     CONN, "            None => (front.get_mut(old..).unwrap_or_default(), back),",
     "            None => (back, front.get_mut(old..).unwrap_or_default()),", [RING]),
    ("M12 no setsockopt: muxer.rs set_buffer() (Unix)",
     MUXER, "let set = unsafe { libc::setsockopt(socket, libc::SOL_SOCKET, option, (&raw const want).cast(), size) };",
     "let set = {\n        let _ = (want, size);\n        0\n    };", [SOCKETS]),
    ("M13 a Linux VM process sizes its host sockets: SIZES_SOCKETS true",
     MUXER, 'const SIZES_SOCKETS: bool = cfg!(not(target_os = "linux"));',
     "const SIZES_SOCKETS: bool = true;", [SOCKETS]),
    ("M14 the relay's socket sized without its maximum first: engine.rs ask_window()",
     ENGINE, "for option in [SO_VM_SOCKETS_BUFFER_MAX_SIZE, SO_VM_SOCKETS_BUFFER_SIZE] {",
     "for option in [SO_VM_SOCKETS_BUFFER_SIZE] {",
     ["engine::tests::the_relays_socket_asks_for_the_hosts_window"]),
]


def run(tree, command, env):
    done = subprocess.run(command, cwd=tree, env=env, capture_output=True, text=True)
    return done.returncode, done.stdout + done.stderr


def failures(output):
    failed = re.findall(r"^test (\S+) \.\.\. FAILED$", output, re.M)
    lines = output.splitlines()
    said = []
    for i, line in enumerate(lines):
        if "panicked at" in line:
            said += [l for l in lines[i : i + 4] if l.strip()]
    return failed, said


def main():
    import os

    repo, scratch = Path(sys.argv[1]).resolve(), Path(sys.argv[2])
    shutil.rmtree(scratch, ignore_errors=True)
    tree = scratch / "repo"
    tree.mkdir(parents=True)
    subprocess.run(
        ["rsync", "-a", "--exclude", "/target", "--exclude", "/.git", f"{repo}/", f"{tree}/"],
        check=True,
    )
    env = dict(os.environ, CARGO_TARGET_DIR=str(scratch / "target"))
    killed = 0
    for what, path, guard, replacement, expected in MUTANTS:
        file = tree / path
        original = file.read_text()
        if original.count(guard) != 1:
            print(f"== {what}\n   guard found {original.count(guard)} times: not run")
            continue
        file.write_text(original.replace(guard, replacement))
        code, output = run(tree, INIT if path == ENGINE else VMM, env)
        file.write_text(original)
        failed, said = failures(output)
        missed = [t for t in expected if not any(f.endswith(t) for f in failed)]
        verdict = "killed" if code != 0 and not missed else "SURVIVED"
        killed += verdict == "killed"
        print(f"== {what}\n   exit {code}; {verdict}; failed: {', '.join(failed) or 'none'}")
        if missed:
            print(f"   expected red, not red: {', '.join(missed)}")
        if code != 0 and not failed:
            print("   " + "\n   ".join(output.splitlines()[-25:]))
        for line in said:
            print(f"   | {line}")
    print(f"== {killed} of {len(MUTANTS)} mutants killed")
    # Restored: the tree as committed, green.
    for name, command in [("vmm", VMM), ("init", INIT)]:
        code, output = run(tree, command, env)
        result = re.findall(r"^test result: .*$", output, re.M)
        print(f"== restored {name}: exit {code}; {' / '.join(result)}")


main()

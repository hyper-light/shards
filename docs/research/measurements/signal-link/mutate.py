#!/usr/bin/env python3
"""Applies one named mutation (an exact replacement) to a copy of the tree: argv[1] the
tree, argv[2] the mutation's name. Exits 2 where its text is not found exactly once."""
import sys, pathlib

WORKLOAD = "crates/shards/src/workload.rs"
HELLOS = "crates/shards/src/workload/hellos.rs"
RUN = "crates/init/src/run.rs"
MUTATIONS = {
    # A send that fails is told it went, as it was at d35c4cf79.
    "HM1-failed-send-told-sent": (WORKLOAD,
        "            Ok(()) => return Ok(()),\n            Err(e) => Lost::of(&e),\n",
        "            Ok(()) | Err(_) => return Ok(()),\n"),
    # A lost link takes frames as if they would go: they queue for ever, as they did.
    "HM2-lost-link-takes-frames": (WORKLOAD,
        "        Link::Lost(lost) => return Err(Unsent::Lost(*lost)),\n",
        "        Link::Lost(_) => return Ok(()),\n"),
    # Nothing reads the connection's end, as nothing did.
    "HM3-no-watch": (WORKLOAD,
        "    let mut header = [0u8; run::HEADER];\n    match shards_ipc::Polled(conn).read_exact(&mut header) {\n",
        "    let mut header = [0u8; run::HEADER];\n    if true {\n        let _ = (to, conn);\n        return;\n    }\n    match shards_ipc::Polled(conn).read_exact(&mut header) {\n"),
    # The connection taken, its end is not watched.
    "HM4-taken-unwatched": (WORKLOAD,
        "        return lose(to, Lost::of(&e));\n    }\n    watch(to, &watched);\n}\n",
        "        return lose(to, Lost::of(&e));\n    }\n    drop(watched);\n}\n"),
    # A loss fails no exec that waits.
    "HM5-loss-fails-no-exec": (WORKLOAD,
        "    let failed = hellos::fail_execs(&why.to_string());\n",
        "    let failed = 0;\n"),
    # The guest's said end is taken for a loss.
    "HM6-end-taken-for-a-loss": (WORKLOAD,
        "        Ok(()) if run::parse_header(header) == Some((kind::SIGNALS_END, 0)) => ended(to),\n",
        "        Ok(()) if run::parse_header(header) == Some((kind::SIGNALS_END, 0)) => lose(to, Lost::Closed),\n"),
    # A send that failed as the guest closed after its end keeps the link lost.
    "HM7-end-keeps-a-loss": (WORKLOAD,
        "        if !matches!(state.link, Link::Connected(_) | Link::Lost(_)) {\n",
        "        if !matches!(state.link, Link::Connected(_)) {\n"),
    # Every frame waits, none in the place of the one it stands for.
    "HM8-no-coalescing": (WORKLOAD,
        "            .find(|(w, p)| *w == which && stands_for(*w, p) == held)\n",
        "            .find(|(w, p)| *w == which && stands_for(*w, p) == held && false)\n"),
    # An exec's signal or size waits without its exec.
    "HM9-exec-frames-unchecked": (WORKLOAD,
        "        if matches!(which, kind::EXEC_SIGNAL | kind::EXEC_RESIZE) {\n",
        "        if false && matches!(which, kind::EXEC_SIGNAL | kind::EXEC_RESIZE) {\n"),
    # Any number is taken for a signal.
    "HM10-any-signal-number": (WORKLOAD,
        "    if (1..=run::NSIG).contains(&linux) {\n",
        "    if linux > 0 || (1..=run::NSIG).contains(&linux) {\n"),
    # Every wait fails as the link goes, a restored run's and a capture's too.
    "HM11-every-wait-fails": (HELLOS,
        "        .extract_if(.., |x| x.target.id().is_some())\n",
        "        .extract_if(.., |_| true)\n"),
    # The guest says no end.
    "GM1-no-end-said": (RUN,
        "    let _ = write(fd, &run::header(kind::SIGNALS_END, 0));\n",
        "    let _ = fd;\n"),
    # The guest says its end only where it saw its connect finish.
    "GM2-connect-not-waited": (RUN,
        "    if !connected {\n        let mut p = libc::pollfd {\n",
        "    if !connected {\n        return;\n    }\n    if !connected {\n        let mut p = libc::pollfd {\n"),
}

def apply(tree, path, old, new):
    p = pathlib.Path(tree) / path
    s = p.read_text()
    if s.count(old) != 1:
        sys.exit(f"{path}: {old!r} found {s.count(old)} times")
    p.write_text(s.replace(old, new))

tree, name = sys.argv[1], sys.argv[2]
apply(tree, *MUTATIONS[name])
print(f"applied {name}")

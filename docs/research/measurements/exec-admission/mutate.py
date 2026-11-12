#!/usr/bin/env python3
"""Applies one named mutation (an exact replacement) to a copy of the tree: argv[1] the
tree, argv[2] the mutation's name. Exits 2 where its text is not found exactly once."""
import sys, pathlib

HELLOS = "crates/shards/src/workload/hellos.rs"
RUN = "crates/init/src/run.rs"
MUTATIONS = {
    # Host: admission refuses while the waiters' lock is busy, as it did.
    "HM1-busy-refuses": (HELLOS,
        "    let admitted = door().admit(conn);\n",
        "    if WAITERS.try_lock().is_err() {\n        return Admission::Closed;\n    }\n    let admitted = door().admit(conn);\n"),
    # Host: admitted connections go with a waiter that stops waiting, as they did.
    "HM2-arrivals-go-with-a-waiter": (HELLOS,
        "    waiters().take(|w| w.target.id() == Some(id))\n",
        "    door().arrived.clear();\n    waiters().take(|w| w.target.id() == Some(id))\n"),
    # Host: past the waiters' number, the newest goes rather than the one silent longest.
    "HM3-newest-evicted": (HELLOS,
        "            .min_by_key(|(_, c)| c.heard)\n",
        "            .max_by_key(|(_, c)| c.heard)\n"),
    # Guest: no kill of a lost exec's process.
    "GM1-no-kill": (RUN,
        "            unsafe { libc::kill(self.pid, libc::SIGKILL) };\n",
        "            let _ = self.pid;\n"),
    # Guest: the host told nothing.
    "GM2-not-told": (RUN,
        "        exec_failed(\n            failures,\n            self.id,\n",
        "        exec_failed(\n            &mut Outbox::default(),\n            self.id,\n"),
    # Guest: an exec a restore reset is told and killed too.
    "GM3-no-generation-guard": (RUN,
        "        if self.born != born || generation() != born {\n            return;\n        }\n",
        "        let _ = (born, &generation);\n"),
    # Guest: an ended exec lets go of a connection the host has not spoken on, as it did.
    "GM4-old-finished": (RUN,
        "                Link::Dialing(_) | Link::Unheard(_) => false,\n",
        "                Link::Dialing(_) | Link::Unheard(_) => self.ended && self.to_conn.is_empty() && !self.joined,\n"),
}
# Guest: the fix reverted whole, as origin/main behaves (its three losses silent, finished as before).
REVERT = [
    ("                Err(e) => self.lose(&format!(\"connecting: {e}\"), failures, born, generation),\n",
     "                Err(_) => self.link = Link::Gone,\n"),
    ("                Err(e) if matches!(self.link, Link::Unheard(_)) => {\n                    self.lose(&format!(\"writing: {e}\"), failures, born, generation);\n                    return;\n                }\n", ""),
    ("                Ok(0) if matches!(self.link, Link::Unheard(_)) => {\n                    self.lose(\"the host closed it\", failures, born, generation);\n                }\n                Err(e) if matches!(self.link, Link::Unheard(_)) => {\n                    self.lose(&format!(\"reading: {e}\"), failures, born, generation);\n                }\n", ""),
    ("                Link::Dialing(_) | Link::Unheard(_) => false,\n",
     "                Link::Dialing(_) | Link::Unheard(_) => self.ended && self.to_conn.is_empty() && !self.joined,\n"),
]

def apply(tree, path, old, new):
    p = pathlib.Path(tree) / path
    s = p.read_text()
    if s.count(old) != 1:
        sys.exit(f"{path}: {old!r} found {s.count(old)} times")
    p.write_text(s.replace(old, new))

tree, name = sys.argv[1], sys.argv[2]
if name == "GM5-fix-reverted":
    for old, new in REVERT:
        apply(tree, RUN, old, new)
else:
    apply(tree, *MUTATIONS[name])
print(f"applied {name}")

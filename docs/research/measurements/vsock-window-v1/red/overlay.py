"""Overlays vsock-window-v1's tests, as committed, on a tree of the code before them.

Usage: overlay.py REPO TREE VARIANT

REPO is a checkout of p/vsock-window-v1; TREE is `git archive` of the base (1e448c7e), with
root-start.patch applied for VARIANT `root`. Each named item (its doc comments and
attributes, then its braces balanced) is copied out of REPO's conn.rs and muxer.rs into the
end of TREE's test modules, with shims for what the older code lacks: a check that always
holds where the test calls one, and on the base the window constants it does not name.
Nothing of the older production code changes.
"""

import re
import sys
from pathlib import Path

VSOCK = Path("crates/vmm/src/devices/virtio/vsock")

CONN_ITEMS = [
    "send",
    "a_window_grows_only_once_its_guest_uses_it",
    "a_window_grows_no_further_than_the_guests_buffer",
    "a_window_never_takes_back_what_it_gave",
    "a_guest_whose_buffer_shrinks_is_owed_its_update",
]
MUXER_HELPERS = ["PACKETS", "pattern", "served_every", "Peer", "impl Peer", "hear"]
MUXER_TESTS = {
    "base": ["a_lone_connection_is_offered_the_most_window_and_reset_past_it"],
    "root": [
        "a_lone_connection_is_offered_the_most_window_and_reset_past_it",
        "two_connections_share_the_lent_window",
        "a_busy_device_keeps_its_window_bounds",
    ],
}
IMPORTS = {
    "base": "    use super::super::packet::MAX_PAYLOAD;\n"
    "    const WINDOW_LEAST: u32 = 64 * 1024;\n"
    "    const WINDOW_MOST: u32 = 4 << 20;\n",
    "root": "    use super::super::conn::{WINDOW_LEAST, WINDOW_MOST};\n"
    "    use super::super::packet::MAX_PAYLOAD;\n",
}
CHECK_SHIM = "    impl {ty} {{\n        fn check(&self) -> Result<(), String> {{\n            Ok(())\n        }}\n    }}\n"


def item(lines, name):
    """The lines of item `name`: its doc comments and attributes, then its body."""
    if name.startswith("impl "):
        pattern = re.compile(r"^\s*" + re.escape(name) + r"\s*\{")
    else:
        pattern = re.compile(r"^\s*(pub )?(fn|struct|const|static) " + re.escape(name) + r"\b")
    starts = [i for i, line in enumerate(lines) if pattern.match(line)]
    if len(starts) != 1:
        raise SystemExit(f"{name}: {len(starts)} matches")
    start = end = starts[0]
    while start > 0 and lines[start - 1].lstrip().startswith(("///", "#[")):
        start -= 1
    depth = 0
    opened = False
    for end in range(starts[0], len(lines)):
        depth += lines[end].count("{") - lines[end].count("}")
        opened |= "{" in lines[end]
        if (opened and depth == 0) or (not opened and lines[end].rstrip().endswith(";")):
            break
    return lines[start : end + 1]


def overlay(source, target, names, extra):
    lines = source.read_text().splitlines(keepends=True)
    block = ["\n", "    // Overlaid from p/vsock-window-v1 (overlay.py).\n", extra]
    for name in names:
        block += ["\n"] + item(lines, name)
    text = target.read_text().rstrip()
    if not text.endswith("}"):
        raise SystemExit(f"{target}: does not end with its test module")
    target.write_text(text[:-1] + "".join(block) + "}\n")


def main():
    repo, tree, variant = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
    shim = CHECK_SHIM.format(ty="Muxer")
    overlay(
        repo / VSOCK / "muxer.rs",
        tree / VSOCK / "muxer.rs",
        MUXER_HELPERS + MUXER_TESTS[variant],
        IMPORTS[variant] + shim,
    )
    if variant == "root":
        overlay(repo / VSOCK / "conn.rs", tree / VSOCK / "conn.rs", CONN_ITEMS, CHECK_SHIM.format(ty="Conn"))


main()

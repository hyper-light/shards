"""Item 4's variants of crates/init/src/domains.rs for mutate4.sh, written into DIR: `red`, the
skeleton at BASE with the new test kept; `no-copy`, the owner and mode copy removed;
`root-0755`, the skeleton's root options removed. Run from a checkout's root with the change
applied: `python3 -I mutants4.py BASE DIR`.
"""

import os
import subprocess
import sys

PATH = "crates/init/src/domains.rs"
START = "/// What a domain mounts over and the image may lack, made where it does: the run makes\n"
END = "/// A domain's root: the image as built, under what [`skeleton`] adds, an overlay attached\n"
COPY = """        let Some(st) = image(path).filter(is_dir) else {
            return Ok(());
        };"""
ROOT = """    let skel = detached(
        c"tmpfs",
        &[
            (c"mode", mode.as_c_str()),
            (c"uid", uid.as_c_str()),
            (c"gid", gid.as_c_str()),
        ],
    )?;"""


def swap(text, old, new):
    if text.count(old) != 1:
        raise SystemExit(f"mutation site not found once: {old[:60]!r}")
    return text.replace(old, new)


def main():
    base_rev, out = sys.argv[1], sys.argv[2]
    head = open(PATH).read()
    base = subprocess.run(["git", "show", f"{base_rev}:{PATH}"], check=True, capture_output=True, text=True).stdout
    a, b = head.index(START), head.index(END)
    ba, bb = base.index(START), base.index(END)
    variants = {
        "red": head[:a] + base[ba:bb] + head[b:],
        "no-copy": swap(head, COPY, COPY.replace("Some(st)", "Some(_st)") + "\n        if true {\n            return Ok(());\n        }\n        let st = _st;"),
        "root-0755": swap(head, ROOT, '    let _ = (&mode, &uid, &gid);\n    let skel = detached(c"tmpfs", &[(c"mode", c"0755")])?;'),
    }
    os.makedirs(out, exist_ok=True)
    for name, text in variants.items():
        with open(os.path.join(out, f"{name}.rs"), "w") as f:
            f.write(text)
        print(name)


if __name__ == "__main__":
    main()

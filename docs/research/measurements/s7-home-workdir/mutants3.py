"""Item 3's guard-removal variants, written into DIR as `<name>/<path>` files that replace
the change's own: run from a checkout's root with the change applied,
`python3 -I mutants3.py DIR`. mutate3.sh lays each over the change and runs the test that
must fail.
"""

import os
import sys

MUTANTS = {
    # The shallowest mount holding HOME taken, not the deepest.
    "shallowest-mount": ("crates/init/src/domains.rs",
                         """        .max_by_key(|(_, p)| p.len())?;""",
                         """        .min_by_key(|(_, p)| p.len())?;"""),
    # A HOME written with `..` placed as written.
    "dotdot-home": ("crates/init/src/domains.rs",
                    """    if comps.contains(&"..") {
        return None;
    }""",
                    ""),
    # A path staged twice written twice.
    "twice-written": ("crates/init/src/staged.rs",
                      """    if let Some(f) = staged.iter().find(|f| !paths.insert(f.path)) {""",
                      """    if let Some(f) = staged.iter().find(|f| !paths.insert(f.path) && false) {"""),
    # An earlier run's file left in the way.
    "no-replace": ("crates/init/src/staged.rs",
                   """    if unsafe { libc::unlinkat(at, name.as_ptr(), 0) } != 0 {""",
                   """    if false && unsafe { libc::unlinkat(at, name.as_ptr(), 0) } != 0 {"""),
    # A link on the way followed.
    "link-followed": ("crates/init/src/staged.rs",
                      """            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,""",
                      """            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,"""),
}


def main():
    out = sys.argv[1]
    for name, (path, old, new) in MUTANTS.items():
        text = open(path).read()
        if text.count(old) != 1:
            raise SystemExit(f"{name}: mutation site not found once in {path}")
        dest = os.path.join(out, name, path)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        with open(dest, "w") as f:
            f.write(text.replace(old, new))
        print(name, path)


if __name__ == "__main__":
    main()

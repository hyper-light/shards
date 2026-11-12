"""Item 2's guard-removal variants, written into DIR as `<name>/<path>` files that replace
the change's own: run from a checkout's root with the change applied,
`python3 -I mutants2.py DIR`. mutate2.sh lays each over the change and runs the test that
must fail.
"""

import os
import sys

MUTANTS = {
    # A relative working directory taken as one.
    "relative-taken": ("crates/abi/src/run/members.rs",
                       """        Some(rest) => path.len() < PATH_MOST && path_ok(rest),
        None => false,""",
                       """        Some(rest) => path.len() < PATH_MOST && path_ok(rest),
        None => path_ok(path),"""),
    # The client's check removed.
    "no-client-check": ("crates/api/src/image.rs",
                        """        if let Some(w) = &m.workdir
            && !shards_abi::run::members::workdir_ok(w.as_bytes())
        {""",
                        """        if let Some(w) = &m.workdir
            && false
        {"""),
    # A second working directory for one member staged by the run host.
    "twice-staged": ("crates/shards/src/api_run.rs",
                     """            if !worked.insert(info.index) {""",
                     """            if !worked.insert(info.index) && false {"""),
    # Init's reading of the entries skipped.
    "guest-ignores": ("crates/init/src/staged.rs",
                      """    members::workdir_of(setup, member)
        .map_err(|e| format!("a staged working directory: {e}"))?""",
                      """    members::workdir_of(&[], member)
        .map_err(|e| format!("a staged working directory: {e}"))?"""),
    # Init's hold on the image's own run.workdir removed.
    "image-workdir-escapes": ("crates/init/src/domains.rs",
                              """    if !inside(workdir) {""",
                              """    if !inside(workdir) && false {"""),
    # The request's working directories not read back.
    "wire-unread": ("crates/api/src/image/wire.rs",
                    """        for m in &mut members {
            m.workdir = if r.flag()? { Some(r.text()?) } else { None };
        }""",
                    ""),
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

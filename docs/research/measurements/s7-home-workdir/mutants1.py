"""Item 1's guard-removal variants, written into DIR as `<name>/<path>` files that replace
the change's own: run from a checkout's root with the change applied,
`python3 -I mutants1.py DIR`. mutate1.sh lays each over the change and runs the test that
must fail.
"""

import os
import sys

MUTANTS = {
    # The image's ENV given no member.
    "no-image-env": ("crates/init/src/domains.rs",
                     """        if !matches!(name.as_str(), "PATH" | "HOME") {
            put(e, &name);
        }""",
                     """        if false {
            put(e, &name);
        }"""),
    # The image's HOME taken as any other name.
    "image-home": ("crates/init/src/domains.rs",
                   """matches!(name.as_str(), "PATH" | "HOME")""",
                   """matches!(name.as_str(), "PATH")"""),
    # The program found on a PATH other than the one it runs with.
    "path-not-its-own": ("crates/init/src/domains.rs",
                         """        .find_map(|e| e.to_bytes().strip_prefix(b"PATH="))
        .unwrap_or_default();""",
                         """        .find_map(|e| e.to_bytes().strip_prefix(b"NOT_PATH="))
        .unwrap_or_default();"""),
    # The build's check removed.
    "no-build-check": ("crates/agentfile/src/plan.rs",
                       """            crate::agentfile::image_env(&t.agentfile, &image.config.env)
                .map_err(|e| Fail::new(e.to_string().into_bytes()))?;""",
                       ""),
    # The daemon's entries never made.
    "no-daemon-entries": ("crates/shards/src/agentfile.rs",
                          """    if agentfile.domains == 0 {
        return Ok(Vec::new());
    }""",
                          """    if agentfile.domains < u32::MAX {
        return Ok(Vec::new());
    }"""),
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

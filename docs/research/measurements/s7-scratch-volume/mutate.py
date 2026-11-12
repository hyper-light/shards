"""Red, green and guard-removal runs for the scratch's checks (plan step 7, D109).

Run from a checkout's root with the change applied: `python3 -I mutate.py BASE`, BASE
being the revision before it. Each case rewrites its files, runs its tests, records
whether they failed, and restores the files whatever happens. "red" is the code at BASE
under the new tests. The guards are the checks at each end, each removed alone. The
environment is the caller's (SHARDS_HELPERS=skip, prebuilt init and server: no test here
starts either).
"""

import os
import subprocess
import sys

BUILD = ["cargo", "test", "-j16", "-p", "shards-agentfile", "--test", "agentfile",
         "volumes_take_a_name_options_and_whom_they_are_for"]
DAEMON = ["cargo", "test", "-j16", "-p", "shards", "--bin", "shards", "--",
          "agentfile::tests::an_images_agentfile_is_read_from_its_root_and_checked",
          "volumes::tests::an_agentfiles_volumes_are_registered_as_it_and_the_run_say"]
INIT = ["cargo", "test", "-j16", "-p", "shards-init", "--", "domains::tests::"]

AGENTFILE = "crates/agentfile/src/agentfile.rs"
SHARDS_AGENTFILE = "crates/shards/src/agentfile.rs"
VOLUMES = "crates/shards/src/volumes.rs"
DOMAINS = "crates/init/src/domains.rs"

DAEMON_CHECK = """        if let Some(m) = af::domain_mount(p.as_bytes()) {
            return Err(fail(format!("{m}: {p}")));
        }
"""
DAEMON_CHECK_BASE = """        if af::covers_domain_mounts(p.as_bytes()) {
            return Err(fail(format!("{}: {p}", af::COVERS)));
        }
"""
VOLUMES_GUARD = "shards_agentfile::agentfile::domain_mount(dest.as_bytes()).is_none()"
VOLUMES_GUARD_BASE = "!shards_agentfile::agentfile::covers_domain_mounts(dest.as_bytes())"
BUILD_SCRATCH = """    } else if within(b"/tmp") {
        Some(DomainMount::Scratch)
"""
INIT_SCRATCH = """    } else if within(path, "/tmp") {
        Some(DomainMount::Scratch)
"""
INIT_DECLARED = """        (None, Some(m)) if declared => Err(m),
"""


def base(rev, path):
    return subprocess.run(["git", "show", f"{rev}:{path}"], check=True, capture_output=True, text=True).stdout


def cases(rev):
    """Each: name, {path: rewrite of its text}, the tests that must fail."""
    return [
        ("red: the code at the base, the new tests", {
            AGENTFILE: lambda _: base(rev, AGENTFILE),
            SHARDS_AGENTFILE: lambda t: swap(t, DAEMON_CHECK, DAEMON_CHECK_BASE),
            VOLUMES: lambda t: swap(t, VOLUMES_GUARD, VOLUMES_GUARD_BASE),
        }, [BUILD, DAEMON]),
        ("the build's /tmp guard removed", {
            AGENTFILE: lambda t: swap(t, BUILD_SCRATCH, ""),
        }, [BUILD, DAEMON]),
        ("the daemon's re-check removed", {
            SHARDS_AGENTFILE: lambda t: swap(t, DAEMON_CHECK, ""),
        }, [DAEMON]),
        ("the daemon's Docker VOLUME guard removed", {
            VOLUMES: lambda t: swap(t, VOLUMES_GUARD, "true"),
        }, [DAEMON]),
        ("init's /tmp guard removed", {
            DOMAINS: lambda t: swap(t, INIT_SCRATCH, ""),
        }, [INIT]),
        ("init's refusal of a declared volume led into /tmp removed", {
            DOMAINS: lambda t: swap(t, INIT_DECLARED, ""),
        }, [INIT]),
    ]


def swap(text, old, new):
    if text.count(old) != 1:
        raise SystemExit(f"mutation site not found once: {old!r}")
    return text.replace(old, new)


def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    out = (r.stdout + r.stderr).splitlines()
    keep = [l for l in out if l.startswith(("test ", "test result", "error", "thread")) or "panicked" in l]
    return r.returncode, keep


def main():
    rev = sys.argv[1]
    print(f"base {rev}, head {subprocess.run(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip()}"
          f" with the change in the index", flush=True)
    for name, rewrites, must_fail in cases(rev):
        saved = {p: open(p).read() for p in rewrites}
        try:
            for p, f in rewrites.items():
                open(p, "w").write(f(saved[p]))
            print(f"== {name}", flush=True)
            for cmd in must_fail:
                code, keep = run(cmd)
                verdict = "FAILED, as it must" if code != 0 else "PASSED: the guard is untested"
                print(f"  {cmd[4]}: exit {code}, {verdict}")
                for l in keep[-12:]:
                    print(f"    {l}")
        finally:
            for p, t in saved.items():
                open(p, "w").write(t)
    print("== green: the change whole", flush=True)
    for cmd in (BUILD, DAEMON, INIT):
        code, keep = run(cmd)
        print(f"  {cmd[4]}: exit {code}, {'passed' if code == 0 else 'FAILED'}")
        for l in keep[-14:]:
            print(f"    {l}")


if __name__ == "__main__":
    main()

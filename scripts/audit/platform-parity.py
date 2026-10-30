#!/usr/bin/env python3
"""Platform parity, the user's criterion (2026-10-27): every feature built for Windows, macOS
and Linux, on amd64 and arm64 (macOS on x86_64 aside). Lists each item a source file compiles
for some hosts alone -- a `#[cfg(unix)]`, `#[cfg(not(windows))]`, `#[cfg(target_os = ...)]` or
`#[cfg(windows)]` before a fn, mod, struct, enum, impl, const, static, type or use -- that no
item of its name in its file, nor a twin platform-parity.tsv names for it, is compiled for on
the hosts it misses, so that what stands alone on one host is found. Test code (`#[cfg(test)]`
modules, tests/ and benches/) is left out, and so are imports (`use`), which do no work of
their own, and the guest's crates, which run in the Linux microVM on every host: shards-init,
the test guest and the in-VM server.

A twin is the item that does another host's part under a name of its own (a Unix transport's
FIFOs and Windows' named pipes, macOS' grants and Windows'): a line of platform-parity.tsv,

    FILE<TAB>KIND NAME<TAB>TWIN FILE<TAB>TWIN KIND NAME<TAB>WHAT THEY DO

stands an item with its twin, which must be there, compiled for the hosts the item misses.
A twin compiled with no cfg of its own in a module only some hosts build (a backend's) names
the hosts it serves: TWIN KIND NAME [HOST,HOST]. A twin that does none of the item's work (a
stub) is none: the item stands alone until its work is done there.
An item whose work the other hosts have no need of (a Linux page cache's fault-around, which
Hypervisor.framework's stage 2 never does) says so in their place, its reason with it:

    FILE<TAB>KIND NAME<TAB>-<TAB>none: HOST[,HOST]<TAB>WHY THEY NEED NONE

    platform-parity.py [ROOT]   one line a lone item: file:line, its cfg, its kind and name,
                                and the hosts it misses; exits 1 while any stands alone
"""
import os, re, sys

HOSTS = {"linux", "macos", "windows"}
SIDES = {
    "unix": {"linux", "macos"},
    "not(windows)": {"linux", "macos"},
    "windows": {"windows"},
    "not(unix)": {"windows"},
    'target_os = "linux"': {"linux"},
    'target_os = "macos"': {"macos"},
    'target_os = "windows"': {"windows"},
    'not(target_os = "linux")': {"macos", "windows"},
    'not(target_os = "macos")': {"linux", "windows"},
}
GUEST = ("crates/init/", "crates/testguest/", "crates/server/")
CFG = re.compile(r'^\s*#\[cfg\((' + "|".join(re.escape(s) for s in SIDES) + r')\)\]\s*$')
ITEM = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?(fn|mod|struct|enum|impl|const|static|type|use|trait)\b\s*([^\s<({:;=]*)')

def items(path):
    """Each item of `path` outside its tests, at any depth: its line, its cfg (None where it
    has none of SIDES' forms), its kind and its name."""
    lines = open(path, errors="replace").read().split("\n")
    out, in_test = [], False
    for i, line in enumerate(lines):
        if re.match(r'^\s*#\[cfg\(test\)\]', line) or re.match(r'^\s*#\[cfg\(all\(test', line):
            in_test = True
        if in_test:
            continue
        m = ITEM.match(line)
        if not m:
            continue
        cfg, j = None, i - 1
        while j >= 0 and (lines[j].strip().startswith(("#[", "//")) or not lines[j].strip()):
            c = CFG.match(lines[j])
            if c:
                cfg = c.group(1)
            if not lines[j].strip():
                break
            j -= 1
        out.append((i + 1, cfg, m.group(1), m.group(2) or line.strip()[:60]))
    return out

def sides_of(cfg):
    return HOSTS if cfg is None else SIDES[cfg]

cache = {}

def defs(path):
    if path not in cache:
        cache[path] = items(path) if os.path.exists(path) else []
    return cache[path]

def hosts_of(path, key, everywhere):
    """The hosts item `key` (KIND NAME) of `path` is compiled for, over its items of that name
    with a cfg of SIDES' forms, and, where `everywhere`, those with none (a twin named in
    platform-parity.tsv may be compiled everywhere; an item's own name, matched alone, is
    held to its cfg'd namesakes, as `fn new` of another type is no twin)."""
    found = set()
    for _, cfg, kind, name in defs(path):
        if f"{kind} {name}" == key and (cfg is not None or everywhere):
            found |= sides_of(cfg)
    return found

here = os.path.dirname(os.path.abspath(__file__))
twins = {}
for n, raw in enumerate(open(os.path.join(here, "platform-parity.tsv"), errors="replace"), 1):
    line = raw.rstrip("\n")
    if not line or line.startswith("#"):
        continue
    parts = line.split("\t")
    if len(parts) < 5:
        print(f"platform-parity.tsv:{n}: needs FILE, KIND NAME, TWIN FILE, TWIN KIND NAME and what they do", file=sys.stderr)
        sys.exit(2)
    twins.setdefault((parts[0], parts[1]), []).append((parts[2], parts[3]))

root = sys.argv[1] if len(sys.argv) > 1 else "crates"
lone = 0
for dirpath, _, files in os.walk(root):
    if "/tests" in dirpath or dirpath.endswith("/benches") or any(g in dirpath + "/" for g in GUEST):
        continue
    for f in sorted(files):
        if not f.endswith(".rs"):
            continue
        path = os.path.join(dirpath, f)
        its = [i for i in defs(path) if i[1] is not None and i[2] != "use"]
        for line, cfg, kind, name in its:
            key = f"{kind} {name}"
            covered = hosts_of(path, key, False)
            for twin_path, twin_key in twins.get((path, key), []):
                if twin_path == "-" and twin_key.startswith("none: "):
                    covered |= set(twin_key[len("none: "):].split(",")) & HOSTS
                    continue
                only = HOSTS
                if twin_key.endswith("]") and " [" in twin_key:
                    twin_key, served = twin_key[:-1].rsplit(" [", 1)
                    only = set(served.split(",")) & HOSTS
                there = hosts_of(twin_path, twin_key, True) & only
                if not there:
                    print(f"platform-parity.tsv: {path}'s {key} names {twin_path}'s {twin_key}, which is not there", file=sys.stderr)
                covered |= there
            missing = HOSTS - covered
            if missing:
                lone += 1
                print(f"{path}:{line}\t{cfg}\t{key}\tmissing: {','.join(sorted(missing))}")
print(f"# {lone} items compiled for some hosts alone, with no twin of their name in their file nor in platform-parity.tsv", file=sys.stderr)
sys.exit(1 if lone else 0)

#!/usr/bin/env python3
"""Platform parity, the user's criterion (2026-10-27): every feature built for Windows, macOS
and Linux, on amd64 and arm64 (macOS on x86_64 aside). Lists each item a source file compiles
for some hosts alone -- a cfg of `unix`, `windows` or `target_os = ...`, or any(), all() or
not() of them, before a fn, mod, struct, enum, impl, const, static, type or use -- that no
item of its name in its file, nor a twin platform-parity.tsv names for it, is compiled for on
the hosts it misses, so that what stands alone on one host is found. Test code (`#[cfg(test)]`
modules, in their file or in a file of their own, tests/ and benches/) is left out, and so are
imports (`use`), which do no work of
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
                                and the hosts it misses; exits 1 while any stands alone, or
                                while a line of platform-parity.tsv names an item no longer
                                in its file
"""
import os, re, sys

HOSTS = {"linux", "macos", "windows"}
GUEST = ("crates/init/", "crates/testguest/", "crates/server/")
CFG = re.compile(r'^\s*#\[cfg\((.*)\)\]\s*$')
ITEM = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?(fn|mod|struct|enum|impl|const|static|type|use|trait)\b\s*([^\s<({:;=]*)')

def args_of(args):
    """The arguments of a cfg's any(), all() or not(), split at their own commas."""
    out, depth, start = [], 0, 0
    for i, c in enumerate(args):
        depth += {"(": 1, ")": -1}.get(c, 0)
        if c == "," and depth == 0:
            out.append(args[start:i])
            start = i + 1
    out.append(args[start:])
    return [a.strip() for a in out if a.strip()]

def hosts(cfg):
    """The hosts a cfg's condition holds on outside tests, of `unix`, `windows` and
    `target_os = ...` and any(), all() and not() of them, `test` holding on none; None
    where it names anything else (an architecture, a backend, a feature), which the audit
    takes as every host's, as it takes an item with no cfg."""
    cfg = cfg.strip()
    if cfg == "unix":
        return {"linux", "macos"}
    if cfg == "windows":
        return {"windows"}
    if cfg == "test":
        return set()
    os_ = re.fullmatch(r'target_os\s*=\s*"(\w+)"', cfg)
    if os_:
        return {os_.group(1)} & HOSTS
    m = re.fullmatch(r'(any|all|not)\((.*)\)', cfg, re.S)
    if not m:
        return None
    sets = [hosts(a) for a in args_of(m.group(2))]
    if not sets or any(h is None for h in sets):
        return None
    if m.group(1) == "not":
        return HOSTS - sets[0] if len(sets) == 1 else None
    if m.group(1) == "any":
        return set().union(*sets)
    return set(HOSTS).intersection(*sets)

def items(path):
    """Each item of `path` outside its tests, at any depth: its line, its cfg (None where it
    has none whose hosts [`hosts`] are known), its kind and its name."""
    lines = open(path, errors="replace").read().split("\n")
    # A test's item (`#[cfg(test)]`, `#[cfg(all(test, ...))]`) is passed over to its end: its
    # line's `;`, or its block's closing brace at its own indentation, as rustfmt writes it.
    out, skip_to, block_end = [], -1, None
    for i, line in enumerate(lines):
        if block_end is not None:
            if line == block_end:
                block_end = None
            continue
        if i <= skip_to:
            continue
        if re.match(r'^\s*#\[cfg\((all\()?test\b', line):
            k = i + 1
            while k < len(lines) and (lines[k].strip().startswith(("#[", "//")) or not lines[k].strip()):
                k += 1
            while k < len(lines) and not lines[k].rstrip().endswith(("{", ";")):
                k += 1
            if k < len(lines) and lines[k].rstrip().endswith("{") and not lines[k].rstrip().endswith("{}"):
                item = lines[k]
                block_end = item[: len(item) - len(item.lstrip())] + "}"
            skip_to = k
            continue
        m = ITEM.match(line)
        if not m:
            continue
        cfg, j = None, i - 1
        while j >= 0 and (lines[j].strip().startswith(("#[", "//")) or not lines[j].strip()):
            c = CFG.match(lines[j])
            if c and hosts(c.group(1)) is not None:
                cfg = c.group(1) if cfg is None else f"all({c.group(1)}, {cfg})"
            if not lines[j].strip():
                break
            j -= 1
        out.append((i + 1, cfg, m.group(1), m.group(2) or line.strip()[:60]))
    return out

TEST_MOD = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;')
PATH_ATTR = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]\s*$')

def test_files(path):
    """The files `path` declares test modules of (`#[cfg(test)]`, or `all(test, ...)`, before
    `mod NAME;`): `#[path]`'s, beside `path`, or where rustc looks for NAME, in `path`'s
    directory where it is a mod.rs, lib.rs or main.rs, else in the directory of its name."""
    lines = open(path, errors="replace").read().split("\n")
    out = []
    for i, line in enumerate(lines):
        m = TEST_MOD.match(line)
        if not m:
            continue
        attrs, j = [], i - 1
        while j >= 0 and lines[j].strip().startswith(("#[", "//")):
            attrs.append(lines[j])
            j -= 1
        if not any(re.match(r'^\s*#\[cfg\((all\()?test\b', a) for a in attrs):
            continue
        here = os.path.dirname(path)
        given = next((PATH_ATTR.match(a).group(1) for a in attrs if PATH_ATTR.match(a)), None)
        if given:
            out.append(os.path.normpath(os.path.join(here, given)))
            continue
        stem = os.path.splitext(os.path.basename(path))[0]
        base = here if stem in ("mod", "lib", "main") else os.path.join(here, stem)
        out += [os.path.join(base, m.group(1) + ".rs"), os.path.join(base, m.group(1), "mod.rs")]
    return out

def sides_of(cfg):
    return HOSTS if cfg is None else hosts(cfg)

cache = {}

def defs(path):
    if path not in cache:
        cache[path] = items(path) if os.path.exists(path) else []
    return cache[path]

def hosts_of(path, key, everywhere):
    """The hosts item `key` (KIND NAME) of `path` is compiled for, over its items of that name
    with a cfg whose hosts are known, and, where `everywhere`, those with none (a twin named in
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
walked = []
for dirpath, _, files in os.walk(root):
    if "/tests" in dirpath or dirpath.endswith("/benches") or any(g in dirpath + "/" for g in GUEST):
        continue
    walked += [os.path.join(dirpath, f) for f in sorted(files) if f.endswith(".rs")]
# A test module's file is test code, and so is every file it declares a module of.
tests, todo = set(), [t for p in walked for t in test_files(p)]
while todo:
    t = os.path.normpath(todo.pop())
    if t not in tests and os.path.exists(t):
        tests.add(t)
        todo += [os.path.join(os.path.splitext(t)[0], n) for n in os.listdir(os.path.splitext(t)[0])
                 if n.endswith(".rs")] if os.path.isdir(os.path.splitext(t)[0]) else []
stale = 0
for path, key in twins:
    if path.startswith(root) and os.path.exists(path) and not any(f"{k} {n}" == key for _, _, k, n in defs(path)):
        stale += 1
        print(f"platform-parity.tsv: {path}'s {key} is no longer there", file=sys.stderr)
lone = 0
for path in walked:
    if os.path.normpath(path) in tests:
        continue
    # An item no host compiles outside its tests (`all(test, unix)`) is a test's.
    its = [i for i in defs(path) if i[1] is not None and i[2] != "use" and hosts(i[1])]
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
sys.exit(1 if lone or stale else 0)

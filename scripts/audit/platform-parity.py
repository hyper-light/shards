#!/usr/bin/env python3
"""Platform parity, the user's criterion (2026-10-27): every feature built for Windows, macOS
and Linux, on amd64 and arm64 (macOS on x86_64 aside). Lists each item a source file compiles
for some hosts alone -- a `#[cfg(unix)]`, `#[cfg(not(windows))]`, `#[cfg(target_os = ...)]` or
`#[cfg(windows)]` before a fn, mod, struct, enum, impl, const, static, type or use -- with
whether the same name is compiled for the other hosts in that file (its twin), so that what
stands alone on one host is found. Test code (`#[cfg(test)]` modules) is left out.

    platform-parity.py [ROOT]   one line a lone item: file:line, its cfg, its kind and name
"""
import os, re, sys

root = sys.argv[1] if len(sys.argv) > 1 else "crates"
CFG = re.compile(r'^\s*#\[cfg\((unix|not\(windows\)|windows|not\(unix\)|target_os = "(linux|macos|windows)"|not\(target_os = "(linux|macos)"\))\)\]\s*$')
ITEM = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?(fn|mod|struct|enum|impl|const|static|type|use|trait)\b\s*([^\s<({:;=]*)')
UNIXISH = {"unix", "not(windows)", 'target_os = "linux"', 'target_os = "macos"', 'not(target_os = "linux")', 'not(target_os = "macos")'}

def items(path):
    lines = open(path, errors="replace").read().split("\n")
    out, in_test, depth_test = [], False, None
    for i, line in enumerate(lines):
        if re.match(r'^\s*#\[cfg\(test\)\]', line) or re.match(r'^\s*#\[cfg\(all\(test', line):
            in_test = True
        m = CFG.match(line)
        if not m or in_test:
            continue
        j = i + 1
        while j < len(lines) and (lines[j].strip().startswith("#[") or lines[j].strip().startswith("///") or not lines[j].strip()):
            j += 1
        if j >= len(lines):
            continue
        it = ITEM.match(lines[j])
        if it:
            out.append((i + 1, m.group(1), it.group(1), it.group(2) or lines[j].strip()[:60]))
    return out

lone = 0
for dirpath, _, files in os.walk(root):
    if "/tests" in dirpath or dirpath.endswith("/benches"):
        continue
    for f in sorted(files):
        if not f.endswith(".rs"):
            continue
        path = os.path.join(dirpath, f)
        its = items(path)
        names = {}
        for line, cfg, kind, name in its:
            side = "unix" if cfg in UNIXISH else "windows"
            names.setdefault((kind, name), set()).add(side)
        for line, cfg, kind, name in its:
            sides = names[(kind, name)]
            if len(sides) == 1:
                lone += 1
                print(f"{path}:{line}\t{cfg}\t{kind} {name}")
print(f"# {lone} items compiled for some hosts alone, with no twin of their name in their file", file=sys.stderr)

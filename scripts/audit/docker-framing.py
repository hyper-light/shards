#!/usr/bin/env python3
"""Every place shards speaks in Docker's terms where it should speak its own, by kind.

    scripts/audit/docker-framing.py [--list] [KIND...]

Without --list it prints a count for each kind and each file; with it, every line. The
kinds are what the microVM-first rule bans (the user, 2026-10-13 to 2026-10-16):

  names     code and test names after Docker, moby, BuildKit, buildx, runc or containerd
  framing   shards' own behaviour said as theirs: "as dockerd does", "in Docker's words"
  container "container" in shards' own voice, where it means a microVM
  grammar   commands said in Docker's form, not `shards ACTION THING`
  strings   text users see that names Docker in shards' own voice
  sockets   sockets shards makes, where it is filesystem based

Recordings (testdata/, the oracles' answers) keep the names of what they record: they are
evidence, not framing, and are not counted.
"""
import os
import re
import sys
from collections import Counter

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SKIP_DIRS = {"target", ".git", "testdata", "vendor", "node_modules"}
CODE = (".rs",)
DOCS = ("README.md", "docs/design/architecture.md", "docs/architecture/AGENTFILE_ARCH.md")

THEIRS = r"(Docker|dockerd|docker/cli|Docker's|dockerd's|docker|buildx|buildx's|BuildKit|BuildKit's|moby|moby's|runc|runc's|containerd|containerd's|the Docker CLI|Docker Desktop)"
KINDS = {
    "names": re.compile(
        r"\b(fn|struct|enum|const|static|mod|type|trait)\s+[A-Za-z0-9_]*"
        r"(dockerd|dockers_|_dockers\b|moby|as_docker|as_buildkit|buildkits|dind|in_dockers_words|as_runc|as_containerd)[A-Za-z0-9_]*"
    ),
    "framing": re.compile(r"\b(as|like|in) " + THEIRS + r"\b|\b(Docker|dockerd|BuildKit|buildx|runc)'s words\b"),
    # Not a path's or a name's part: `cli/command/container/cp.go`, `docker-container`.
    "container": re.compile(r"(?<![/\-`])\b(container|containers)\b(?![/\-`])"),
    "grammar": re.compile(
        r"`shards (?:--context \S+ )?(run|ps|rm|rmi|images|cp|stop|kill|logs|wait|exec|inspect|start|create|build|pull|push|save|load|tag|commit|export|import|attach|top|stats|events|diff|history|port|pause|unpause|rename|restart|update|checkpoint)\b"
        r"(?! (vm|vms|microvm|container|image|images|volume|volumes|daemon|guest|disk|system|agent|harness|mcp|agentfile|registry|snapshot|snapshots|context|contexts|client|clients|invitation|invitations)\b)"
    ),
    "strings": re.compile(r'"[^"\n]*\b(Docker|dockerd)\b[^"\n]*"'),
    "sockets": re.compile(r"\b(UnixListener::bind|\.sock\b|listening_at|bind\(&path\))"),
}


def files():
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".")]
        for f in filenames:
            path = os.path.relpath(os.path.join(dirpath, f), ROOT)
            if path.endswith(CODE) or path in DOCS:
                yield path


def comment_or_doc(path, line):
    return not path.endswith(".rs") or "//" in line


def hits(kind):
    rx = KINDS[kind]
    for path in files():
        try:
            lines = open(os.path.join(ROOT, path), encoding="utf-8", errors="replace").read().split("\n")
        except OSError:
            continue
        for n, line in enumerate(lines, 1):
            if not rx.search(line):
                continue
            # "container" counts where shards speaks: comments and docs, not code's own words.
            if kind == "container" and not comment_or_doc(path, line):
                continue
            if kind == "strings" and line.lstrip().startswith("//"):
                continue
            yield path, n, line.strip()


def main():
    args = sys.argv[1:]
    listing = "--list" in args
    kinds = [a for a in args if a != "--list"] or list(KINDS)
    for kind in kinds:
        found = list(hits(kind))
        print(f"== {kind}: {len(found)}")
        if listing:
            for path, n, line in found:
                print(f"{path}:{n}: {line[:200]}")
        else:
            for path, count in Counter(p for p, _, _ in found).most_common():
                print(f"{count:6d}  {path}")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Hold s37-decision-inventory.md to the mains it cites (plan step 37's closing check).

Every commit the inventory cites (a backticked run of 7 to 12, or 40, hex digits) must name
one commit of exactly one of the repositories given, and be an ancestor of that repository's
main, unless the text beside it says it is off main (the only way a commit no clone holds
passes); one written right after a repository's name must be that repository's. Every
D-number must be an entry of Shards' docs/design/architecture.md at its main (a row of its
decisions table or a section its heading names), and every PM M-number one of
docs/research/platform-measurements.md there. Every "open" in a table row's Status names who
decides it: "open, ROOT decides", "open, the person decides" or "open, the Hex lead decides".
kiln goal and gate ids (a backticked id after "goal" or "gate") are not commits.

  s37-decision-inventory-check.py --repo shards=PATH:REF --repo kiln=PATH:REF \
      --repo hex=PATH:REF --repo hyper-raft=PATH:REF [INVENTORY]

INVENTORY defaults to the inventory beside this script. Exits 0 when every check holds, 1
otherwise, printing each failure and the counts.
"""

import argparse
import pathlib
import re
import subprocess
import sys

SHA = re.compile(r"`([0-9a-f]{7,12}|[0-9a-f]{40})`")
# A kiln goal or gate id: `goal 3268f6d8`, "ROOT's goal `c627cc94`", "gate `22dfc46c`".
NOT_A_COMMIT = re.compile(r"\b(?:goal|gate)s? `?$")
# What the text says of a commit it cites off its repository's main.
# "off Hex main", "not yet on Hex's main", "off hyper-raft's main", "never on Hex main", "off its main".
OFF_MAIN = re.compile(r"\b(?:not (?:yet )?on|off|never on) (?:[\w-]+(?:'s)? )?main\b")
# How near the off-main words must be: within the clause the commit sits in.
NEAR = 200
DNUM = re.compile(r"\bD(\d+)\b")
PM = re.compile(r"\bPM ((?:M\d+)(?:(?:, | and |, and )M\d+)*)")
# An open decision names who decides it, in a table row's Status (its last cell).
OPEN = re.compile(r"\bopen\b(?!, (?:ROOT|the person|the Hex lead) decides)")
LABEL = re.compile(r"\b(Shards|kiln|Hex|hyper-raft)(?:'s)?(?: main| GitHub main)? \(?`?$")


def git(path, *args, stdin=None):
    return subprocess.run(
        ["git", "-C", path, *args], input=stdin, capture_output=True, text=True, check=True
    ).stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", action="append", required=True, help="NAME=PATH:REF")
    ap.add_argument("inventory", nargs="?",
                    default=str(pathlib.Path(__file__).with_name("s37-decision-inventory.md")))
    a = ap.parse_args()

    repos = {}
    for spec in a.repo:
        name, rest = spec.split("=", 1)
        path, ref = rest.rsplit(":", 1)
        head = git(path, "rev-parse", "--verify", ref + "^{commit}").strip()
        on_main = set(git(path, "rev-list", head).split())
        repos[name] = (path, ref, head, on_main)
        print(f"{name}: {ref} = {head} ({len(on_main)} commits)")

    text = pathlib.Path(a.inventory).read_text()
    failures = []

    cited = []
    for m in SHA.finditer(text):
        if NOT_A_COMMIT.search(text[max(0, m.start() - 12):m.start() + 1]):
            continue
        cited.append(m)
    tokens = sorted({m.group(1) for m in cited})

    # Resolve every token in every repository at once.
    where = {t: [] for t in tokens}
    for name, (path, _, _, _) in repos.items():
        out = git(path, "cat-file", "--batch-check=%(objectname) %(objecttype)",
                  stdin="".join(f"{t}^{{commit}}\n" for t in tokens)).splitlines()
        for t, line in zip(tokens, out):
            parts = line.split()
            if len(parts) == 2 and parts[1] == "commit":
                where[t].append((name, parts[0]))
            elif "ambiguous" in line:
                where[t].append((name, "ambiguous"))

    on_main = off_said = unseen = 0
    for m in cited:
        t = m.group(1)
        line = text.count("\n", 0, m.start()) + 1
        found = where[t]
        said = OFF_MAIN.search(text[max(0, m.start() - NEAR):m.end() + NEAR])
        if not found:
            # A commit no clone here holds (a lead's local commit, one replaced before it was
            # pushed) passes only where the text says it is off main.
            if said:
                unseen += 1
                continue
            failures.append(f"line {line}: `{t}` names no commit of {', '.join(repos)}")
            continue
        if len(found) > 1 or found[0][1] == "ambiguous":
            failures.append(f"line {line}: `{t}` is ambiguous: {found}")
            continue
        name, full = found[0]
        label = LABEL.search(text[max(0, m.start() - 30):m.start() + 1])
        if label and label.group(1).lower() != name:
            failures.append(f"line {line}: `{t}` labelled {label.group(1)}, a commit of {name}")
        if full in repos[name][3]:
            on_main += 1
            continue
        if said:
            off_said += 1
            continue
        failures.append(f"line {line}: `{t}` ({name}) is not on {name}'s main, and the text "
                        "beside it does not say so")

    rows = opens = 0
    for i, row in enumerate(text.split("\n"), 1):
        if not row.startswith("| ") or row.startswith("|---") or row.startswith("| #") \
                or row.startswith("| Step") or row.startswith("| Goal"):
            continue
        rows += 1
        status = row.rstrip(" |").rsplit(" | ", 1)[-1]
        opens += len(re.findall(r"\bopen\b", status))
        for m in OPEN.finditer(status):
            failures.append(f"line {i}: an open decision names no decider: "
                            f"...{status[max(0, m.start() - 60):m.end() + 30]}...")

    shards_path, _, shards_head, _ = repos["shards"]
    arch = git(shards_path, "show", f"{shards_head}:docs/design/architecture.md")
    pm = git(shards_path, "show", f"{shards_head}:docs/research/platform-measurements.md")
    # A D-entry is a row of the decisions table (`| D12 |`), a section headed by its number
    # (`### D59: ...`, `### D241. ...`) or one whose heading's parenthetical opens with it
    # (`### Guest memory (D29)`, `### Warm pool (D26, in progress)`).
    entries = set(re.findall(
        r"^(?:\| *D(\d+) *\||#{2,4} D(\d+)[.:]|#{2,4} [^\n]*?\(D(\d+)\b)", arch, re.M))
    entries = {d for group in entries for d in group if d}
    measurements = set(re.findall(r"^#{2,4} M(\d+)\.", pm, re.M))
    dnums = sorted({int(n) for n in DNUM.findall(text)})
    for n in dnums:
        if str(n) not in entries:
            failures.append(f"D{n} heads no entry of architecture.md at {shards_head[:9]}")
    mnums = sorted({int(n) for g in PM.findall(text) for n in re.findall(r"M(\d+)", g)})
    for n in mnums:
        if str(n) not in measurements:
            failures.append(f"PM M{n} heads no entry of platform-measurements.md at "
                            f"{shards_head[:9]}")

    for f in failures:
        print("FAIL", f)
    print(f"commits cited: {len(cited)} ({len(tokens)} distinct): {on_main} on their mains, "
          f"{off_said} off main and said so, {unseen} in no clone and said off main; rows: {rows}, "
          f"open decisions: {opens}; D-numbers: {len(dnums)}; PM M-numbers: {len(mnums)}; "
          f"failures: {len(failures)}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

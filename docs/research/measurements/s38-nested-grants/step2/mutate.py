"""Applies one named mutant to crates/agentfile/src/agentfile.rs (run in the clone's root).
Each replacement must match exactly once, so a mutant that no longer applies fails loudly."""
import sys
from pathlib import Path

MUTANTS = {
    # check(): the build's refusal of NESTING beside an agent or harness, removed.
    "check-beside": (
        "            // `NESTING` and a domain in one lineage, in either order: refused at the second.\n"
        "            if seen.nesting\n"
        "                && let Some(refused) = seen.beside()\n"
        "            {\n"
        "                return Err(at(line, refused.to_string()));\n"
        "            }\n",
        "",
    ),
    # check(): the build's refusal of a second NESTING in a lineage, removed.
    "check-twice": (
        "                    if seen.nesting {\n"
        "                        return Err(at(line, NestingRefused::Twice.to_string()));\n"
        "                    }\n",
        "",
    ),
    # nesting(): the daemon's re-check beside a domain, removed.
    "nesting-beside": (
        "    match first(TargetKind::Agent).or_else(|| first(TargetKind::Harness)) {\n"
        "        Some(refused) => Err(refused),\n"
        "        None => Ok(true),\n"
        "    }\n",
        "    let _ = first;\n    Ok(true)\n",
    ),
    # nesting(): the daemon's re-check of a second NESTING, removed.
    "nesting-twice": (
        "        _ => return Err(NestingRefused::Twice),\n",
        "        _ => {}\n",
    ),
    # The line's refusal of words, removed.
    "arguments": (
        "    if !req.args.is_empty() {\n"
        "        return Err(NestingRefused::Arguments.to_string().into_bytes());\n"
        "    }\n",
        "",
    ),
}

name = sys.argv[1]
old, new = MUTANTS[name]
path = Path("crates/agentfile/src/agentfile.rs")
text = path.read_text(encoding="utf-8")
count = text.count(old)
if count != 1:
    sys.exit(f"mutant {name}: its text matches {count} times, not once")
path.write_text(text.replace(old, new), encoding="utf-8")
print(f"mutant {name} applied")

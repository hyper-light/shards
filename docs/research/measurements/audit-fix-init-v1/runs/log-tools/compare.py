import json, sys
# argv[1]: JSON string literal (the tool's verbatim "output"); argv[2]: a saved log whose
# leading "# " lines are annotations. Prints "identical" or a unified diff of the bodies.
import difflib
raw = json.loads(open(sys.argv[1], encoding="utf-8").read())
lines = open(sys.argv[2], encoding="utf-8").read().split("\n")
i = 0
while i < len(lines) and lines[i].startswith("# "):
    i += 1
body = "\n".join(lines[i:])
if body == raw:
    print("identical", sys.argv[2].rsplit("/", 1)[-1])
else:
    print("DIFFERS", sys.argv[2].rsplit("/", 1)[-1])
    sys.stdout.writelines(difflib.unified_diff(raw.splitlines(True), body.splitlines(True), "runner", "saved"))

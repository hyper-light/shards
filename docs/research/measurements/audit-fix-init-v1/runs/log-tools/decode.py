import json, sys
# argv[1]: a file holding one JSON string literal (the kiln tool's "output" field, verbatim);
# argv[2]: header file; argv[3]: destination log. Writes header then the decoded raw output.
raw = json.loads(open(sys.argv[1], encoding="utf-8").read())
with open(sys.argv[3], "w", encoding="utf-8") as out:
    out.write(open(sys.argv[2], encoding="utf-8").read())
    out.write(raw)

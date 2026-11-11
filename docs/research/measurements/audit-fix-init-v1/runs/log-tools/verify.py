import hashlib, json, sys
# argv[1]: JSON string literal of a capture.sh output. Checks the body (all but the last
# "capture-sha256 <hex> exit <n>" line) against that hash; prints the verdict.
raw = json.loads(open(sys.argv[1], encoding="utf-8").read())
body, _, last = raw.rstrip("\n").rpartition("\n")
body += "\n"
want = last.split()[1]
got = hashlib.sha256(body.encode("utf-8")).hexdigest()
print("match" if got == want else f"MISMATCH want {want} got {got}")

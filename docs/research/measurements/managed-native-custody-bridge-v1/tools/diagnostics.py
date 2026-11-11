import re
def headings(text):
    found = [f"{m.group(1)}: {m.group(3)} @ {m.group(4)}" for m in
             re.finditer(r'^(error|warning)(\[[^\]]+\])?: (.*)\n\s*--> ([^\n]+)', text, re.M)]
    return sorted(found)

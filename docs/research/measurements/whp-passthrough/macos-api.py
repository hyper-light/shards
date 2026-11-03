#!/usr/bin/env python3
"""Inventory the installed macOS hypervisor API, read-only (D154, PM M403).

Lists the hv_* names followed by '(' in every public header, including both guest
architectures. Also lists any PCI, IOMMU, DMA, passthrough or assignment mention, so
the evidence can be inspected beyond symbol names. This is an API inventory, not a
test of entitled drivers or private APIs.
"""
from collections import Counter
import hashlib
from pathlib import Path
import re
import subprocess

sdk = Path(subprocess.check_output(["xcrun", "--show-sdk-path"], text=True).strip()).resolve()
print(subprocess.check_output(["sw_vers"], text=True).strip())
print(f"SDK: {sdk}")
headers = sdk / "System/Library/Frameworks/Hypervisor.framework/Headers"
functions = set()
mentions = []
for header in sorted(headers.glob("*.h")):
    data = header.read_bytes()
    print(f"{header.name}: sha256 {hashlib.sha256(data).hexdigest()}")
    source = data.decode()
    functions.update(re.findall(r"\bhv_[a-z0-9_]+\s*(?=\()", source))
    for line, text in enumerate(source.splitlines(), 1):
        if re.search(r"pci|iommu|\bdma\b|passthrough|assign|smmu|dart", text, re.I):
            mentions.append(f"{header.name}:{line}: {text.strip()}")
if not functions:
    raise SystemExit("no Hypervisor.framework function names found")
families = Counter(name.split("_")[1] for name in functions)
print(f"Function names: {len(functions)}")
print("Families:", dict(sorted(families.items())))
print("Mentions:")
print("\n".join(mentions) or "none")
print("Names:")
print("\n".join(sorted(functions)))

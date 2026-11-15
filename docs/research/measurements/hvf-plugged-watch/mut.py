# Mutation of the fix (green build, never committed): the vCPU keeps the copy of the watch's
# regions it was made with, never copying them again.
import sys

p = "crates/vmm/src/hv/hvf/mod.rs"
s = open(p).read()
old = """        if self.watch.changes.load(Ordering::Acquire) != self.memory_as_of {
            (self.memory, self.memory_as_of) = self.watch.regions();
        }
"""
if s.count(old) != 1:
    sys.exit("mutation anchor not found once")
open(p, "w").write(s.replace(old, "        let _ = &self.memory_as_of;\n"))
print("mutated")

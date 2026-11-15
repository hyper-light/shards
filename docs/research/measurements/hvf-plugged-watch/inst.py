# SCRATCH logging for the red build only (never committed): what HVF's watch takes away as a
# recording starts, each plug, each snapshot's plugged runs, and each fault on memory mapped
# after a vCPU was made that the vCPU takes as MMIO. Logging alone: no behaviour changes.
import sys

def edit(path, old, new):
    s = open(path).read()
    if s.count(old) != 1:
        sys.exit(f"{path}: anchor found {s.count(old)} times: {old[:60]!r}")
    open(path, "w").write(s.replace(old, new))

hvf = "crates/vmm/src/hv/hvf/mod.rs"
edit(hvf,
"""        let mut recording = lock(&self.recording);
        *recording = Some(Recording::default());
""",
"""        let mut recording = lock(&self.recording);
        *recording = Some(Recording::default());
        crate::warn!(
            "SCRATCH watch: a recording takes away {:x?}",
            lock(&self.regions).iter().map(|r| (r.gpa, r.len)).collect::<Vec<_>>()
        );
""")
edit(hvf,
"""                esr::EC_DABT_LOW => self.mmio(io, syndrome, ipa)?,
""",
"""                esr::EC_DABT_LOW => {
                    if let Some(r) = self.watch.region_of(ipa) {
                        crate::warn!(
                            "SCRATCH vCPU {}: a fault at {ipa:#x} (ESR {syndrome:#x}) on {:#x}+{:#x}, mapped since this vCPU was made, taken as MMIO",
                            self.index,
                            r.gpa,
                            r.len
                        );
                    }
                    self.mmio(io, syndrome, ipa)?
                }
""")
edit("crates/vmm/src/devices/virtio/mem.rs",
"""            Ok(()) => {
                self.count(len, true);
                ACK
""",
"""            Ok(()) => {
                self.count(len, true);
                crate::warn!("SCRATCH virtio-mem: plugged {addr:#x}+{len:#x}");
                ACK
""")
edit("crates/vmm/src/vm/runtime.rs",
"""        if resume {
            // Before a working set is recorded, which takes the head from the guest too.
""",
"""        warn!("SCRATCH snapshot staged, plugged runs {:x?}", snap.plugged);
        if resume {
            // Before a working set is recorded, which takes the head from the guest too.
""")
print("instrumented")

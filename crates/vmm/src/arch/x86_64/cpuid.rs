//! Per-vCPU CPUID from the hypervisor's template (Intel SDM Vol. 2A CPUID; research doc
//! §1.4). The template carries the host's APIC id and no topology, so each vCPU gets its
//! own APIC id and a flat topology (one thread per core, `count` cores). The hypervisor
//! bit is always set: without it the guest never looks for kvmclock (arch/x86/kernel/
//! kvm.c:887-906).

/// One CPUID leaf/subleaf as the hypervisor reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leaf {
    pub function: u32,
    pub index: u32,
    pub flags: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// The leaf's value depends on the subleaf index (KVM_CPUID_FLAG_SIGNIFCANT_INDEX).
pub const SIGNIFICANT_INDEX: u32 = 1;

const HTT: u32 = 1 << 28;
const HYPERVISOR: u32 = 1 << 31;
const TSC_DEADLINE: u32 = 1 << 24;
const LEVEL_SMT: u32 = 1;
const LEVEL_CORE: u32 = 2;

/// The CPUID vCPU `index` of `count` sees.
pub fn for_vcpu(template: &[Leaf], index: u32, count: u32, tsc_deadline: bool) -> Vec<Leaf> {
    let apic = index;
    let core_bits = 32 - count.saturating_sub(1).leading_zeros(); // ceil(log2(count))
    let mut leaves: Vec<Leaf> = template
        .iter()
        .filter(|l| l.function != 0xb && l.function != 0x1f)
        .copied()
        .collect();
    for l in &mut leaves {
        if l.function == 1 {
            // EBX: APIC id (31:24) and logical processors per package (23:16).
            l.ebx = (l.ebx & 0x0000_ffff) | (apic << 24) | (count.min(255) << 16);
            l.ecx |= HYPERVISOR;
            if tsc_deadline {
                l.ecx |= TSC_DEADLINE;
            } else {
                l.ecx &= !TSC_DEADLINE;
            }
            if count > 1 {
                l.edx |= HTT;
            } else {
                l.edx &= !HTT;
            }
        }
    }
    // Extended topology, if the template has the leaf: SMT (1 thread), then core.
    let max_basic = template.iter().find(|l| l.function == 0).map_or(0, |l| l.eax);
    for function in [0xb, 0x1f] {
        if max_basic < function || !template.iter().any(|l| l.function == function) {
            continue;
        }
        let topo = |index: u32, shift: u32, logical: u32, kind: u32| Leaf {
            function,
            index,
            flags: SIGNIFICANT_INDEX,
            eax: shift,
            ebx: logical,
            ecx: (kind << 8) | index,
            edx: apic,
        };
        leaves.push(topo(0, 0, 1, LEVEL_SMT));
        leaves.push(topo(1, core_bits, count, LEVEL_CORE));
        leaves.push(topo(2, 0, 0, 0));
    }
    leaves
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn leaf(function: u32, eax: u32, ebx: u32, ecx: u32, edx: u32) -> Leaf {
        Leaf {
            function,
            index: 0,
            flags: 0,
            eax,
            ebx,
            ecx,
            edx,
        }
    }

    #[test]
    fn gives_each_vcpu_its_apic_id_and_a_flat_topology() {
        let template = [
            leaf(0, 0x1f, 0, 0, 0),
            leaf(1, 0x806f8, 0xff00_0800, 0x7ffa_3203, 0x0f8b_fbff),
            leaf(0xb, 0, 0, 0, 0),
            leaf(0x1f, 0, 0, 0, 0),
        ];
        let v = for_vcpu(&template, 5, 6, true);
        let l1 = v.iter().find(|l| l.function == 1).unwrap();
        assert_eq!(l1.ebx >> 24, 5);
        assert_eq!(l1.ebx >> 16 & 0xff, 6);
        assert_eq!(l1.ebx & 0xffff, 0x0800); // CLFLUSH size and brand kept
        assert_ne!(l1.ecx & HYPERVISOR, 0);
        assert_ne!(l1.ecx & TSC_DEADLINE, 0);
        assert_ne!(l1.edx & HTT, 0);
        let core = v.iter().find(|l| l.function == 0xb && l.index == 1).unwrap();
        assert_eq!((core.eax, core.ebx, core.ecx >> 8 & 0xff, core.edx), (3, 6, 2, 5));
        assert_eq!(v.iter().filter(|l| l.function == 0x1f).count(), 3);
        let solo = for_vcpu(&template, 0, 1, false);
        let l1 = solo.iter().find(|l| l.function == 1).unwrap();
        assert_eq!((l1.ecx & TSC_DEADLINE, l1.edx & HTT), (0, 0));
    }
}

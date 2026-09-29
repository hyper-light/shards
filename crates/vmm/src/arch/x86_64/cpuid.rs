//! Per-vCPU CPUID from the hypervisor's template (Intel SDM Vol. 2A CPUID; AMD64 APM Vol. 3
//! Appendix E; research doc §1.4). The template carries the host's APIC id, and KVM passes
//! the host's topology through in places: the cores that share each cache (leaves 4 and
//! 0x8000001D) and the threads in the package (0x80000008 ECX), while it zeroes
//! 0x8000001E and leaves 0xB and 0x1F (Linux 6.17 arch/x86/kvm/cpuid.c). So each vCPU gets
//! its own APIC id and one flat topology in every leaf: one package and node, `count`
//! cores of one thread each, private L1 and L2 caches and a shared L3, as Firecracker
//! gives its guests (v1.17.0 src/vmm/src/cpu_config/x86_64/cpuid/{amd,intel}/
//! normalize.rs). The hypervisor bit is always set: without it the guest never looks for
//! kvmclock (arch/x86/kernel/kvm.c:887-906).

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
    let others = count.saturating_sub(1);
    for l in &mut leaves {
        match l.function {
            1 => {
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
            // A cache (type in EAX[4:0], 0 ends the list; level in EAX[7:5]): the logical
            // processors sharing it, less one, in EAX[25:14], and in leaf 4 the cores in the
            // package, less one, in EAX[31:26].
            4 | 0x8000_001d if l.eax & 0x1f != 0 => {
                let sharing = if (l.eax >> 5) & 7 >= 3 { others } else { 0 };
                l.eax = (l.eax & !(0xfff << 14)) | (sharing.min(0xfff) << 14);
                if l.function == 4 {
                    l.eax = (l.eax & !(0x3f << 26)) | (others.min(0x3f) << 26);
                }
            }
            // ECX: the threads in the package, less one (7:0), and the APIC id bits that
            // number them (15:12).
            0x8000_0008 => l.ecx = (l.ecx & !0xf0ff) | (core_bits.min(0xf) << 12) | others.min(0xff),
            // EAX: the extended APIC id. EBX: the core's id (7:0) and its threads, less one
            // (15:8). ECX: the node's id (7:0) and the nodes in the package, less one (10:8).
            0x8000_001e => {
                l.eax = apic;
                l.ebx = index.min(0xff);
                l.ecx = 0;
                l.edx = 0;
            }
            _ => {}
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

    fn subleaf(function: u32, index: u32, eax: u32) -> Leaf {
        Leaf {
            index,
            flags: SIGNIFICANT_INDEX,
            ..leaf(function, eax, 0x01c0_003f, 0x3f, 0)
        }
    }

    /// Caches as a host with two threads per core and 16 cores per package reports them:
    /// type (4:0), level (7:5), self-initializing (8), then the sharing counts.
    fn host_caches(function: u32) -> Vec<Leaf> {
        let cache = |kind: u32, level: u32, sharing: u32| {
            (15 << 26) | (sharing << 14) | (1 << 8) | (level << 5) | kind
        };
        vec![
            subleaf(function, 0, cache(1, 1, 1)),
            subleaf(function, 1, cache(2, 1, 1)),
            subleaf(function, 2, cache(3, 2, 1)),
            subleaf(function, 3, cache(3, 3, 31)),
            subleaf(function, 4, 0),
        ]
    }

    #[test]
    fn caches_are_private_to_a_core_but_the_last_level_which_all_share() {
        for function in [4, 0x8000_001d] {
            let v = for_vcpu(&host_caches(function), 2, 6, false);
            let sharing: Vec<u32> = v.iter().map(|l| l.eax >> 14 & 0xfff).collect();
            assert_eq!(sharing, [0, 0, 0, 5, 0], "{function:#x}");
            let cores: Vec<u32> = v.iter().map(|l| l.eax >> 26).collect();
            let want = if function == 4 { 5 } else { 15 };
            assert_eq!(cores, [want, want, want, want, 0], "{function:#x}");
            // The type, level and the rest of each subleaf are the host's.
            for (got, host) in v.iter().zip(host_caches(function)) {
                assert_eq!(got.eax & 0x3fff, host.eax & 0x3fff);
                assert_eq!((got.ebx, got.ecx, got.edx), (host.ebx, host.ecx, host.edx));
            }
        }
    }

    #[test]
    fn amd_topology_is_one_package_of_single_thread_cores() {
        let template = [
            // A host's 16 threads (NC 15) numbered by 7 APIC id bits, a performance
            // timestamp counter's size (17:16), and its address sizes.
            leaf(0x8000_0008, 0x3030, 0x0100_d005, 0x3_7000 | 15, 0),
            // KVM zeroes 0x8000001E; a host's would name its own core, thread and node.
            leaf(0x8000_001e, 0x2b, 0x0105, 0x0301, 0),
        ];
        let v = for_vcpu(&template, 3, 6, false);
        let sizes = v.iter().find(|l| l.function == 0x8000_0008).unwrap();
        assert_eq!(
            (sizes.ecx & 0xff, sizes.ecx >> 12 & 0xf, sizes.ecx >> 16),
            (5, 3, 3)
        );
        assert_eq!((sizes.eax, sizes.ebx), (0x3030, 0x0100_d005));
        let topo = v.iter().find(|l| l.function == 0x8000_001e).unwrap();
        assert_eq!((topo.eax, topo.ebx, topo.ecx, topo.edx), (3, 3, 0, 0));
        let solo = for_vcpu(&template, 0, 1, false);
        let sizes = solo.iter().find(|l| l.function == 0x8000_0008).unwrap();
        assert_eq!(sizes.ecx & 0xf0ff, 0);
        // Fields saturate rather than wrap on counts too large for them.
        let many = for_vcpu(&host_caches(4), 300, 300, false);
        assert_eq!(many[3].eax >> 26, 0x3f);
        assert_eq!(many[3].eax >> 14 & 0xfff, 299);
    }
}

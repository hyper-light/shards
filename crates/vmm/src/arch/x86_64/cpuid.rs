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

use std::collections::BTreeMap;

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
/// CPUID.01H:ECX[24], TSC-Deadline (Intel SDM Vol. 2A, CPUID, "Feature Information
/// Returned in the ECX Register"; Linux arch/x86/include/asm/cpufeatures.h l. 129,
/// `X86_FEATURE_TSC_DEADLINE_TIMER` = word 4, bit 24).
const TSC_DEADLINE: u32 = 1 << 24;
const LEVEL_SMT: u32 = 1;
const LEVEL_CORE: u32 = 2;

/// XCR0's AMX tile configuration (17) and tile data (18): XSETBV takes them together or
/// not at all (Linux arch/x86/kvm/x86.h, `kvm_get_filtered_xcr0`).
const XTILE_CFG: u32 = 1 << 17;
const XTILE_DATA: u32 = 1 << 18;

/// The physical address width a guest given `template` has: leaf 0x80000008's `EAX[7:0]`
/// (MAXPHYADDR), or 36 where its CPU has no such leaf (Intel SDM Vol. 3A §4.1.4).
pub fn phys_bits(template: &[Leaf]) -> u32 {
    template
        .iter()
        .find(|l| l.function == 0x8000_0008)
        .map_or(36, |l| l.eax & 0xff)
}

/// Takes the tile configuration out of the XCR0 bits leaf 0xD offers where the tile data
/// is not among them (review 1.10). Linux 5.17 to 6.3 offered it so to a process not
/// permitted the data, and a guest that enables what it is offered takes #GP at XSETBV;
/// Linux 6.4 offers neither then (55cd57b596e8, "KVM: x86: Filter out XTILE_CFG if
/// XTILE_DATA isn't permitted").
pub fn without_lone_tile_config(template: &mut [Leaf]) {
    for l in template.iter_mut().filter(|l| l.function == 0xd && l.index == 0) {
        if l.eax & XTILE_DATA == 0 {
            l.eax &= !XTILE_CFG;
        }
    }
}

/// Invariant TSC: CPUID 0x80000007 EDX bit 8 (AMD64 APM Vol. 3 E.4.6; Intel SDM Vol. 2A).
pub const INVARIANT_TSC: u32 = 1 << 8;

/// A CPUID result as a hypervisor that keeps its guests' CPUID takes one (WHP's
/// CpuidResultList2, PM M206): only the bits of `mask` are the result's, the rest stay the
/// hypervisor's own; for one vCPU, or for every vCPU (`vcpu` None).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Masked {
    pub function: u32,
    pub index: u32,
    /// The result is the subleaf's alone ([`SIGNIFICANT_INDEX`]).
    pub subleaf: bool,
    pub vcpu: Option<u32>,
    pub value: [u32; 4],
    pub mask: [u32; 4],
}

/// What each of `vcpus` vCPUs is given of [`for_vcpu`], told to a hypervisor that already
/// gives every vCPU `template` (with each vCPU's own APIC id in it): each leaf only in the
/// bits that differ from `template`, so that the bits the hypervisor keeps per vCPU and as
/// the guest runs (OSXSAVE, the APIC's enable) stay its own; with invariant TSC where
/// `invariant_tsc`. A result every vCPU has alike is told once, for all of them.
pub fn masked(template: &[Leaf], vcpus: u32, invariant_tsc: bool) -> Vec<Masked> {
    let mut by_leaf: BTreeMap<(u32, u32), Vec<Masked>> = BTreeMap::new();
    for vcpu in 0..vcpus {
        let mut leaves = for_vcpu(template, vcpu, vcpus, false);
        if invariant_tsc {
            for l in leaves.iter_mut().filter(|l| l.function == 0x8000_0007) {
                l.edx |= INVARIANT_TSC;
            }
        }
        for l in leaves {
            let mask = match template
                .iter()
                .find(|t| t.function == l.function && t.index == l.index)
            {
                Some(t) => [l.eax ^ t.eax, l.ebx ^ t.ebx, l.ecx ^ t.ecx, l.edx ^ t.edx],
                None => [!0; 4],
            };
            if mask == [0; 4] {
                continue;
            }
            by_leaf.entry((l.function, l.index)).or_default().push(Masked {
                function: l.function,
                index: l.index,
                subleaf: l.flags & SIGNIFICANT_INDEX != 0,
                vcpu: Some(vcpu),
                value: [l.eax, l.ebx, l.ecx, l.edx],
                mask,
            });
        }
    }
    let mut results = Vec::new();
    for (_, each) in by_leaf {
        let alike = each.len() == vcpus as usize
            && each
                .windows(2)
                .all(|w| matches!(w, [a, b] if (a.value, a.mask) == (b.value, b.mask)));
        match each.first() {
            Some(first) if alike => results.push(Masked { vcpu: None, ..*first }),
            _ => results.extend(each),
        }
    }
    results
}

/// Whether the guest may use KVM's TSC-deadline timer after applying its CPU baseline.
pub fn deadline_timer(supported: bool, _baseline: &[[u32; 7]]) -> bool {
    supported
}

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

    /// The tile configuration goes where the tile data is not offered, and stays where it
    /// is; nothing else changes (review 1.10).
    #[test]
    fn a_lone_tile_configuration_is_not_offered() {
        let xcr0 = |eax| Leaf {
            function: 0xd,
            index: 0,
            flags: SIGNIFICANT_INDEX,
            eax,
            ebx: 0x2b00,
            ecx: 0x2b00,
            edx: 0,
        };
        let x87_sse_avx = 0x7;
        // Leaf 0xD's other subleaves hold other things in EAX, bit 17 among them maybe.
        let mut leaves = [
            xcr0(x87_sse_avx | XTILE_CFG),
            Leaf {
                index: 1,
                ..xcr0(XTILE_CFG | 0xf)
            },
            Leaf {
                index: 17,
                ..xcr0(64)
            },
            leaf(7, 0, 0, 0, 1 << 24),
        ];
        let before = leaves;
        without_lone_tile_config(&mut leaves);
        assert_eq!(leaves[0], xcr0(x87_sse_avx));
        assert_eq!(leaves[1..], before[1..]);
        for whole in [x87_sse_avx | XTILE_CFG | XTILE_DATA, x87_sse_avx] {
            let mut leaves = [xcr0(whole)];
            without_lone_tile_config(&mut leaves);
            assert_eq!(leaves, [xcr0(whole)]);
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

    /// CPUID as WHP gives vCPU 0 of a partition set up as shards' are, on an AMD EPYC 7763
    /// under Windows Server 2025 (PM M206).
    fn whp_template() -> Vec<Leaf> {
        let mut v = vec![
            leaf(0, 0xd, 0x6874_7541, 0x444d_4163, 0x6974_6e65),
            leaf(1, 0x00a0_0f11, 0x0000_0800, 0xf6da_3203, 0x078b_fbff),
            leaf(0x8000_0000, 0x8000_0021, 0x6874_7541, 0x444d_4163, 0x6974_6e65),
            leaf(0x8000_0007, 0, 0, 0, 0),
            leaf(0x8000_0008, 0x3030, 0x2000_0015, 0, 0x0001_0000),
            leaf(0x8000_001e, 0, 0, 0, 0),
        ];
        let caches = [
            (0x121, 0x01c0_003f, 0x3f, 0),
            (0x122, 0x01c0_003f, 0x3f, 0),
            (0x143, 0x01c0_003f, 0x3ff, 2),
            (0x163, 0x03c0_003f, 0x7fff, 1),
            (0, 0, 0, 0),
        ];
        for (index, (eax, ebx, ecx, edx)) in (0..).zip(caches) {
            v.push(Leaf {
                function: 0x8000_001d,
                index,
                flags: SIGNIFICANT_INDEX,
                eax,
                ebx,
                ecx,
                edx,
            });
        }
        v
    }

    #[test]
    fn a_hypervisor_that_keeps_cpuid_is_told_only_the_bits_that_change() {
        let m = masked(&whp_template(), 4, true);
        let find = |function, index, vcpu| {
            m.iter()
                .find(|r| r.function == function && r.index == index && r.vcpu == vcpu)
        };
        // Invariant TSC, the package's four threads and two APIC id bits, and the L3 all
        // four share: once, for every vCPU.
        let tsc = find(0x8000_0007, 0, None).unwrap();
        assert_eq!(tsc.mask, [0, 0, 0, INVARIANT_TSC]);
        assert_eq!(tsc.value[3], INVARIANT_TSC);
        let threads = find(0x8000_0008, 0, None).unwrap();
        assert_eq!((threads.value[2], threads.mask), (0x2003, [0, 0, 0x2003, 0]));
        let l3 = find(0x8000_001d, 3, None).unwrap();
        assert_eq!(
            (l3.value[0] >> 14 & 0xfff, l3.mask[0], l3.subleaf),
            (3, 3 << 14, true)
        );
        // Leaf 1, for each vCPU: its APIC id where not 0, the count and HTT; OSXSAVE and
        // the rest the hypervisor's.
        for vcpu in 0..4 {
            let l1 = find(1, 0, Some(vcpu)).unwrap();
            assert_eq!(l1.value[1], vcpu << 24 | 4 << 16 | 0x800);
            assert_eq!(l1.mask, [0, vcpu << 24 | 4 << 16, 0, HTT]);
        }
        // 0x8000001E: vCPU 0's is the hypervisor's own; the others name their cores.
        assert!(find(0x8000_001e, 0, Some(0)).is_none());
        let topo = find(0x8000_001e, 0, Some(3)).unwrap();
        assert_eq!((topo.value, topo.mask), ([3, 3, 0, 0], [3, 3, 0, 0]));
        assert_eq!(m.len(), 3 + 4 + 3, "{m:#x?}");
        // A hypervisor that gives invariant TSC is not told it; one vCPU has no HTT.
        let mut offered = whp_template();
        for l in offered.iter_mut().filter(|l| l.function == 0x8000_0007) {
            l.edx = INVARIANT_TSC;
        }
        let solo = masked(&offered, 1, true);
        assert!(solo.iter().all(|r| r.function != 0x8000_0007));
        assert_eq!(solo.len(), 1, "{solo:#x?}");
        assert_eq!((solo[0].function, solo[0].vcpu), (1, None));
        assert_eq!(solo[0].mask, [0, 1 << 16, 0, 0]);
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

/// The registers of the leaves that say which features a CPU has, by leaf and subleaf,
/// each a register's place in a leaf's `[function, index, flags, eax, ebx, ecx, edx]`
/// (Intel SDM vol. 2A, CPUID; AMD APM vol. 3, appendix E): a set bit in each is a feature
/// the guest may use. Leaf 0DH's subleaf 0 says in EAX and EDX the XCR0 bits the CPU
/// supports, and subleaf 1 in ECX and EDX the IA32_XSS bits, each an XSAVE state component
/// a guest may enable (Intel SDM vol. 1 §13.2, §13.3), as cloud-hypervisor's migration
/// check reads them too (v54.1 arch/src/x86_64/mod.rs, `CpuidFeatureEntry`): one moved to a
/// CPU without a component it enabled could not set it again (KVM_SET_XCRS, the IA32_XSS
/// MSR), nor run what used it. Leaf 1EH's subleaf 1 says in EAX which AMX instructions past
/// the first the CPU has, which KVM offers as features (Linux arch/x86/kvm/cpuid.c,
/// `CPUID_1E_1_EAX`). Leaf 0x40000001's EAX is KVM's paravirtual features (Linux
/// Documentation/virt/kvm/x86/cpuid.rst, KVM_CPUID_FEATURES), which differ from host to
/// host (steal time only where the host keeps scheduler statistics, cpuid.c). Not here:
/// leaves 1DH and 1EH's subleaf 0, AMX's tile sizes, numbers its leaf-7 and XCR0 bits
/// gate; leaf 0x40000000, KVM's signature, every KVM host's alike; leaf 7's EAX, its highest
/// subleaf, as one a host lacks reads as zeros in [`covers`].
const FEATURES: &[(u32, u32, &[u8])] = &[
    (1, 0, &[5, 6]),
    (7, 0, &[4, 5, 6]),
    (7, 1, &[3, 4, 5, 6]),
    (7, 2, &[6]),
    (0xd, 0, &[3, 6]),
    (0xd, 1, &[3, 5, 6]),
    (0x14, 0, &[4, 5]),
    (0x19, 0, &[4]),
    (0x1e, 1, &[3]),
    (0x4000_0001, 0, &[3]),
    (0x8000_0001, 0, &[5, 6]),
    (0x8000_0007, 0, &[6]),
    (0x8000_0008, 0, &[4]),
    (0x8000_0021, 0, &[3]),
];

/// The CPUID leaves that say which CPU a snapshot was taken on, as the Windows Hypervisor
/// Platform describes it to a partition's guest: its vendor and family, its features, its
/// XSAVE components and its address sizes. Not its caches, which a hybrid host's cores
/// describe apart (PM M258). WHP restores a snapshot only where these are the same (hv::whp's
/// state.rs), which a move checks before anything moves (D245).
pub const IDENTITY: &[(u32, u32)] = &[
    (0, 0),
    (1, 0),
    (7, 0),
    (7, 1),
    (0xD, 0),
    (0xD, 1),
    (0x8000_0000, 0),
    (0x8000_0001, 0),
    (0x8000_0007, 0),
    (0x8000_0008, 0),
];

/// The registers [`FEATURES`] names of leaf `function`.`index`; none for another leaf.
fn feature_places(function: u32, index: u32) -> Option<&'static [u8]> {
    FEATURES
        .iter()
        .find(|&&(f, i, _)| f == function && i == index)
        .map(|&(_, _, places)| places)
}

/// Leaf `function`.`index` of `table`, as `[function, index, flags, eax, ebx, ecx, edx]`.
fn leaf_of(table: &[[u32; 7]], function: u32, index: u32) -> Option<[u32; 7]> {
    table.iter().find(|l| l[0] == function && l[1] == index).copied()
}

/// A CPU's vendor, as leaf 0's EBX, EDX and ECX spell it ("GenuineIntel", "AuthenticAMD").
fn vendor(table: &[[u32; 7]]) -> Option<[u32; 3]> {
    leaf_of(table, 0, 0).map(|l| [l[4], l[6], l[5]])
}

/// A feature register a host has less of than its guest found ([`covers`]): its leaf and
/// subleaf, its place in `[function, index, flags, eax, ebx, ecx, edx]` (3, EAX, to 6, EDX),
/// and the bits the guest has set that the host has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short {
    pub function: u32,
    pub index: u32,
    pub register: u8,
    pub missing: u32,
}

impl std::fmt::Display for Short {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = usize::from(self.register)
            .checked_sub(3)
            .and_then(|i| ["eax", "ebx", "ecx", "edx"].get(i).copied())
            .unwrap_or("?");
        write!(
            f,
            "leaf {:#x}.{} {name} bits {:#x}",
            self.function, self.index, self.missing
        )
    }
}

/// Why a host does not run a guest ([`covers`]), or why two CPUs have no CPU in common
/// ([`meet`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lacks {
    /// They are different vendors' CPUs, which no CPUID makes one of the other (Firecracker
    /// v1.18.0 docs/cpu_templates/cpu-templates.md: "Representing one CPU vendor as another
    /// CPU vendor is not supported").
    Vendor,
    /// Each feature register the host has less of.
    Bits(Vec<Short>),
}

impl std::fmt::Display for Lacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lacks::Vendor => f.write_str("this CPU is another vendor's than its guest's"),
            Lacks::Bits(short) => {
                let each: Vec<String> = short.iter().map(Short::to_string).collect();
                write!(f, "this CPU lacks what its guest uses: {}", each.join(", "))
            }
        }
    }
}

/// Whether a vCPU given `host`'s CPUID runs a guest that found `guest`'s (D176 part 6): the
/// same vendor (leaf 0), and every feature bit the guest has set in the host's too. Why not,
/// each register short, where it does not.
pub fn covers(host: &[[u32; 7]], guest: &[[u32; 7]]) -> Result<(), Lacks> {
    if vendor(host) != vendor(guest) {
        return Err(Lacks::Vendor);
    }
    let mut short = Vec::new();
    for &(function, index, places) in FEATURES {
        let Some(g) = leaf_of(guest, function, index) else {
            continue;
        };
        let h = leaf_of(host, function, index).unwrap_or([function, index, 0, 0, 0, 0, 0]);
        for &register in places {
            let at = usize::from(register);
            let (gv, hv) = (g.get(at).copied().unwrap_or(0), h.get(at).copied().unwrap_or(0));
            let missing = gv & !hv;
            if missing != 0 {
                short.push(Short {
                    function,
                    index,
                    register,
                    missing,
                });
            }
        }
    }
    if short.is_empty() {
        Ok(())
    } else {
        Err(Lacks::Bits(short))
    }
}

/// This host's CPU as `template`, its hypervisor's view of it (KVM_GET_SUPPORTED_CPUID's
/// leaves), shows a guest, as a CPU baseline names it ([`baseline`]): leaf 0, which names
/// its vendor, and each leaf of [`FEATURES`], its feature registers alone.
pub fn identity(template: &[Leaf]) -> Vec<[u32; 7]> {
    template
        .iter()
        .filter_map(|l| {
            let whole = [l.function, l.index, 0, l.eax, l.ebx, l.ecx, l.edx];
            if (l.function, l.index) == (0, 0) {
                return Some(whole);
            }
            let places = feature_places(l.function, l.index)?;
            let mut leaf = [l.function, l.index, 0, 0, 0, 0, 0];
            for &p in places {
                let at = usize::from(p);
                if let (Some(slot), Some(&v)) = (leaf.get_mut(at), whole.get(at)) {
                    *slot = v;
                }
            }
            Some(leaf)
        })
        .collect()
}

/// The CPU every one of `cpus` runs ([`meet`] of them all); none of none.
pub fn baseline(cpus: &[Vec<[u32; 7]>]) -> Result<Vec<[u32; 7]>, Lacks> {
    let mut all = cpus.iter();
    let Some(first) = all.next() else {
        return Ok(Vec::new());
    };
    all.try_fold(first.clone(), |acc, cpu| meet(&acc, cpu))
}

/// The CPU both `a` and `b` run: one vendor's, its leaf 0 `a`'s, and of each leaf of
/// [`FEATURES`] either names the feature bits both set, none of a leaf one lacks. As
/// libvirt's `hypervisor-cpu-baseline` makes one of what each host's hypervisor offers a
/// guest (v12.9.0 docs/manpages/virsh.rst), and Linux's own view of a system's CPUs is each
/// feature that all of them have.
pub fn meet(a: &[[u32; 7]], b: &[[u32; 7]]) -> Result<Vec<[u32; 7]>, Lacks> {
    if vendor(a) != vendor(b) {
        return Err(Lacks::Vendor);
    }
    let mut both: Vec<[u32; 7]> = leaf_of(a, 0, 0).into_iter().collect();
    for &(function, index, places) in FEATURES {
        let (la, lb) = (leaf_of(a, function, index), leaf_of(b, function, index));
        if la.is_none() && lb.is_none() {
            continue;
        }
        let bits = |l: Option<[u32; 7]>, at: usize| l.and_then(|l| l.get(at).copied()).unwrap_or(0);
        let mut leaf = [function, index, 0, 0, 0, 0, 0];
        for &p in places {
            let at = usize::from(p);
            if let Some(slot) = leaf.get_mut(at) {
                *slot = bits(la, at) & bits(lb, at);
            }
        }
        both.push(leaf);
    }
    Ok(both)
}

/// `template`, a hypervisor's CPUID, told `baseline` (D176 part 6): of each leaf of
/// [`FEATURES`] the baseline names, only the feature bits the baseline sets too; the rest
/// the hypervisor's. Before [`for_vcpu`], whose bits are the VM's shape, none a host's.
pub fn within(template: &mut [Leaf], baseline: &[[u32; 7]]) {
    for l in template.iter_mut() {
        let (Some(places), Some(b)) = (
            feature_places(l.function, l.index),
            leaf_of(baseline, l.function, l.index),
        ) else {
            continue;
        };
        for &p in places {
            let keep = b.get(usize::from(p)).copied().unwrap_or(0);
            match p {
                3 => l.eax &= keep,
                4 => l.ebx &= keep,
                5 => l.ecx &= keep,
                6 => l.edx &= keep,
                _ => {}
            }
        }
    }
}

/// `id` as text: `FUNCTION.INDEX=EAX:EBX:ECX:EDX` by leaf, in hex, comma-separated, as a
/// home keeps a CPU baseline and a VM process is given one (D176 part 6).
pub fn encode(id: &[[u32; 7]]) -> String {
    id.iter()
        .map(|l| {
            format!(
                "{:x}.{:x}={:08x}:{:08x}:{:08x}:{:08x}",
                l[0], l[1], l[3], l[4], l[5], l[6]
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// [`encode`]'s text read back; empty for none. A baseline names leaf 0 and each leaf of
/// [`FEATURES`] once ([`identity`], [`meet`]), and WHP's CPU the leaves of [`IDENTITY`], so a
/// text of more leaves than either is neither's.
pub fn decode(text: &str) -> Result<Vec<[u32; 7]>, String> {
    let leaves: Vec<&str> = text.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    let most = FEATURES.len().saturating_add(1).max(IDENTITY.len());
    if leaves.len() > most {
        return Err(format!("{} CPUID leaves, more than a CPU's {most}", leaves.len()));
    }
    leaves
        .into_iter()
        .map(|leaf| {
            let bad = || format!("{leaf:?} is no CPUID leaf's FUNCTION.INDEX=EAX:EBX:ECX:EDX, in hex");
            let hex = |s: &str| u32::from_str_radix(s, 16).map_err(|_| bad());
            let (at, regs) = leaf.split_once('=').ok_or_else(bad)?;
            let (function, index) = at.split_once('.').ok_or_else(bad)?;
            let regs: Vec<&str> = regs.split(':').collect();
            let &[eax, ebx, ecx, edx] = regs.as_slice() else {
                return Err(bad());
            };
            Ok([
                hex(function)?,
                hex(index)?,
                0,
                hex(eax)?,
                hex(ebx)?,
                hex(ecx)?,
                hex(edx)?,
            ])
        })
        .collect()
}

#[cfg(test)]
mod covers_tests {
    use super::*;

    const INTEL: [u32; 7] = [0, 0, 0, 0x1f, 0x756e_6547, 0x6c65_746e, 0x4965_6e69];
    const AMD: [u32; 7] = [0, 0, 0, 0x10, 0x6874_7541, 0x444d_4163, 0x6974_6e65];

    /// A host with every feature bit its guest found runs it, more besides; one without a
    /// bit of it, or another vendor's, does not, each said by what it lacks.
    #[test]
    fn a_host_runs_a_guest_whose_every_feature_it_has() {
        let guest = [INTEL, [7, 0, 1, 0, 0x239c_27eb, 0x9840_0784, 0xbc18_4410]];
        let mut more = guest;
        more[1][4] |= 1 << 30;
        assert!(covers(&more, &guest).is_ok());
        let refused = covers(&guest, &more).unwrap_err();
        assert_eq!(
            refused,
            Lacks::Bits(vec![Short {
                function: 7,
                index: 0,
                register: 4,
                missing: 1 << 30
            }])
        );
        assert!(
            refused.to_string().contains("leaf 0x7.0 ebx bits 0x40000000"),
            "{refused}"
        );
        let mut other = guest;
        other[0] = AMD;
        assert_eq!(covers(&other, &guest), Err(Lacks::Vendor));
        assert!(Lacks::Vendor.to_string().contains("vendor"));
    }

    /// Two hosts, one with AVX-512 (leaf 7 EBX bit 16, and its three XSAVE components, XCR0
    /// bits 5 to 7), one with SHA (EBX bit 29) the other lacks: neither runs the other's
    /// guest, and both run their meet's, which has neither, nor a leaf one of them lacks.
    /// Their identity keeps leaf 0 and the features alone: no family, model or APIC ID.
    #[test]
    fn a_baseline_masks_what_any_machine_lacks() {
        let leaf = |function, index, eax, ebx, ecx, edx| Leaf {
            function,
            index,
            flags: 0,
            eax,
            ebx,
            ecx,
            edx,
        };
        let zen = [
            leaf(0, 0, 0x10, AMD[4], AMD[5], AMD[6]),
            leaf(1, 0, 0x00b4_0f40, 0x0010_0800, 0x7ef8_320b, 0x178b_fbff),
            leaf(7, 0, 0, 0x2000_0000 | 0x019c_91a9, 0x0040_068c, 0x0000_0010),
            leaf(0xd, 0, 0x7, 0x340, 0x340, 0),
        ];
        let avx512 = [
            leaf(0, 0, 0x10, AMD[4], AMD[5], AMD[6]),
            leaf(1, 0, 0x00a6_0f12, 0x0020_0800, 0x7ef8_320b, 0x178b_fbff),
            leaf(7, 0, 0, 0x0001_0000 | 0x019c_91a9, 0x0040_068c, 0x0000_0010),
            leaf(7, 1, 0x20, 0, 0, 0),
            leaf(0xd, 0, 0xe7, 0x980, 0x980, 0),
        ];
        let (a, b) = (identity(&zen), identity(&avx512));
        assert_eq!(
            a[1],
            [1, 0, 0, 0, 0, 0x7ef8_320b, 0x178b_fbff],
            "no family, model nor APIC ID"
        );
        assert!(covers(&a, &b).is_err() && covers(&b, &a).is_err());
        let both = baseline(&[a.clone(), b.clone()]).unwrap();
        assert!(covers(&a, &both).is_ok() && covers(&b, &both).is_ok());
        assert_eq!(
            leaf_of(&both, 7, 0).unwrap()[4],
            0x019c_91a9,
            "neither SHA nor AVX-512F"
        );
        assert_eq!(
            leaf_of(&both, 7, 1).unwrap()[3],
            0,
            "a leaf one lacks, none of it"
        );
        assert_eq!(
            leaf_of(&both, 0xd, 0).unwrap()[3],
            0x7,
            "x87, SSE and AVX state alone"
        );
        // Told the baseline, each host's own CPUID keeps the rest as its hypervisor gives it.
        let mut told = avx512;
        within(&mut told, &both);
        assert_eq!((told[1].eax, told[1].ebx), (0x00a6_0f12, 0x0020_0800));
        assert_eq!(told[2].ebx, 0x019c_91a9);
        assert_eq!((told[3].eax, told[4].eax, told[4].ebx), (0, 0x7, 0x980));
        assert!(covers(&a, &identity(&told)).is_ok());
        let intel = [INTEL, [1, 0, 0, 0, 0, 0x7ef8_320b, 0x178b_fbff]];
        assert_eq!(meet(&a, &intel), Err(Lacks::Vendor));
        assert_eq!(baseline(&[]), Ok(Vec::new()));
    }

    #[test]
    fn a_cpu_baseline_keeps_deadline_timer_shaping_within_its_features() {
        let template = vec![leaf(1, 0, 0, TSC_DEADLINE, 0)];
        let baseline = vec![[1, 0, 0, 0, 0, 0, 0]];
        let mut masked = template.clone();
        within(&mut masked, &baseline);
        let guest = for_vcpu(&masked, 0, 1, deadline_timer(true, &baseline));
        assert_eq!(guest[0].ecx & TSC_DEADLINE, 0);
        assert!(deadline_timer(true, &[]));
        assert!(!deadline_timer(false, &[]));
        let allowed = [[1, 0, 0, 0, 0, TSC_DEADLINE, 0]];
        assert!(deadline_timer(true, &allowed));
        assert!(!deadline_timer(false, &allowed));
        assert!(!deadline_timer(true, &[[0, 0, 0, 0, 0, 0, 0]]));
    }

    /// A baseline's text reads back as it was written; one of more leaves than a baseline
    /// names, or of a leaf not in its form, is refused.
    #[test]
    fn a_baseline_reads_back_as_written() {
        let id = vec![
            AMD,
            [7, 0, 0, 0, 0x219c_91a9, 0x0040_068c, 0x10],
            [0xd, 0, 0, 7, 0, 0, 0],
        ];
        let text = encode(&id);
        assert!(
            text.starts_with("0.0=00000010:68747541:444d4163:69746e65,7.0="),
            "{text}"
        );
        assert_eq!(decode(&text).unwrap(), id);
        assert_eq!(decode("").unwrap(), Vec::<[u32; 7]>::new());
        let many = vec![text.as_str(); FEATURES.len()].join(",");
        assert!(decode(&many).unwrap_err().contains("more than"));
        for bad in ["7.0=1:2:3", "7=1:2:3:4", "7.0=1:2:3:4:5", "c021=0000000000000000"] {
            assert!(decode(bad).is_err(), "{bad}");
        }
    }

    /// What a guest may use beyond the instruction-set leaves is a baseline's too: a
    /// supervisor XSAVE component (IA32_XSS bit 11, CET's user state), an AMX instruction of
    /// leaf 1EH's subleaf 1 (bit 4) and KVM's steal time (KVM_FEATURE_STEAL_TIME, bit 5),
    /// which one host has and the other not. Neither host runs the other's guest, each such
    /// register named; their meet has none of the three, and both run it.
    #[test]
    fn a_baseline_holds_supervisor_state_amx_and_kvms_own_features() {
        let leaf = |function, index, eax, ebx, ecx, edx| Leaf {
            function,
            index,
            flags: 0,
            eax,
            ebx,
            ecx,
            edx,
        };
        let host = |xss: u32, amx: u32, kvm: u32| {
            identity(&[
                leaf(0, 0, 0x10, AMD[4], AMD[5], AMD[6]),
                leaf(0xd, 1, 0xf, 0, xss, 0),
                leaf(0x1e, 1, amx, 0, 0, 0),
                leaf(0x4000_0001, 0, kvm, 0, 0, 0),
            ])
        };
        let (cet, fp8, steal) = (1 << 11, 1 << 4, 1 << 5);
        // Some of KVM's paravirtual features (cpuid.rst's flags 0-16), steal time not one.
        let paravirt = 0x1_4edb;
        assert_eq!(paravirt & steal, 0);
        let more = host(0x100 | cet, 0x2 | fp8, paravirt | steal);
        let less = host(0x100, 0x2, paravirt);
        assert_eq!(more.len(), 4, "identity keeps each leaf: {more:?}");
        assert_eq!(
            covers(&less, &more),
            Err(Lacks::Bits(vec![
                Short {
                    function: 0xd,
                    index: 1,
                    register: 5,
                    missing: cet,
                },
                Short {
                    function: 0x1e,
                    index: 1,
                    register: 3,
                    missing: fp8,
                },
                Short {
                    function: 0x4000_0001,
                    index: 0,
                    register: 3,
                    missing: steal,
                },
            ]))
        );
        let both = baseline(&[more.clone(), less.clone()]).unwrap();
        assert!(covers(&less, &both).is_ok() && covers(&more, &both).is_ok());
        assert_eq!(both, less);
    }
}

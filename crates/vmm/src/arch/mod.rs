//! Guest architectures.

pub mod aarch64;
pub mod x86_64;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// The guest kernel takes as many CPUs as a machine of each architecture has vCPUs at
    /// its most (CONFIG_NR_CPUS, resources/kernel/shards-<arch>.config, merged over
    /// Firecracker's base, whose 64 it replaces), so that a machine given one vCPU for
    /// each host CPU (D191) runs them all.
    #[test]
    fn the_guest_kernel_takes_every_vcpu_a_machine_has() {
        let kernel = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/kernel");
        let nr_cpus = |file: &str| -> u32 {
            let text = std::fs::read_to_string(kernel.join(file)).unwrap();
            let mut said = text.lines().filter_map(|l| l.strip_prefix("CONFIG_NR_CPUS="));
            let n = said.next().unwrap().parse().unwrap();
            assert_eq!(said.next(), None, "{file} names NR_CPUS once");
            n
        };
        assert_eq!(nr_cpus("shards-x86_64.config"), super::x86_64::acpi::MAX_VCPUS);
        assert_eq!(
            nr_cpus("shards-aarch64.config"),
            super::aarch64::layout::MAX_VCPUS
        );
        // Each base's own, which they replace, is below them.
        assert!(nr_cpus("firecracker-x86_64-6.18.config") < super::x86_64::acpi::MAX_VCPUS);
        assert!(nr_cpus("firecracker-aarch64-6.18.config") < super::aarch64::layout::MAX_VCPUS);
        // Within Kconfig's ranges (arch/x86/Kconfig NR_CPUS_RANGE_END without
        // CPUMASK_OFFSTACK; arch/arm64/Kconfig NR_CPUS), which olddefconfig keeps.
        assert!((2..=512).contains(&super::x86_64::acpi::MAX_VCPUS));
        assert!((2..=4096).contains(&super::aarch64::layout::MAX_VCPUS));
    }
}

# A guest that is a KVM host (nested virtualization on Apple silicon)

shards' arm64 KVM backend is developed and tested in a guest booted at EL2 by shards'
own Hypervisor.framework backend: `hv_vm_config_get_el2_supported` answers yes on this
Mac (Apple M5 Max, macOS 26.4.1), and `hv_vm_config_set_el2_enabled` (macOS 15) turns
EL2 on (`run --kernel --nested`, D132). The kernel it boots is the pinned kernel's `kvm`
variant (resources/kernel/kvm.config: KVM, and the virtualization menu it sits in), which
the kernel workflow builds and releases beside the others.

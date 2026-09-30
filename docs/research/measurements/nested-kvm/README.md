# A guest that is a KVM host (nested virtualization on Apple silicon)

shards' arm64 KVM backend is developed and tested in a guest booted at EL2 by shards'
own Hypervisor.framework backend: `hv_vm_config_get_el2_supported` answers yes on this
Mac (Apple M5 Max, macOS 26.4.1), and `hv_vm_config_set_el2_enabled` (macOS 15) turns
EL2 on. `.github/workflows/nested-kvm.yml` builds the pinned arm64 kernel in the pinned
builder with `kvm.config` added (KVM, and the virtualization menu it sits in) and uploads
it as an artifact: nothing is released.

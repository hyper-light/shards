// Whether a Hypervisor.framework guest given EL2 (nested virtualization, macOS 15 on M3 and
// later) has VHE: for a VM of
// EL2 off and one of EL2 on, what its vCPU's ID registers say (ID_AA64MMFR1_EL1.VH, bits
// 11:8; ID_AA64MMFR4_EL1.E2H0, bits 27:24, where the framework has the register), and
// whether HCR_EL2.E2H (bit 34) holds a write. PM M422, run by run.sh.
#include <Hypervisor/Hypervisor.h>
#include <stdio.h>

static void show(const char *what, hv_vcpu_t vcpu, hv_sys_reg_t reg) {
    uint64_t v = 0;
    hv_return_t r = hv_vcpu_get_sys_reg(vcpu, reg, &v);
    printf("  %-18s %#018llx (ret %#x)\n", what, (unsigned long long)v, r);
}

static int one(bool el2) {
    hv_vm_config_t config = hv_vm_config_create();
    if (el2) {
        hv_return_t r = hv_vm_config_set_el2_enabled(config, true);
        printf("el2 on: set_el2_enabled ret %#x\n", r);
    } else {
        printf("el2 off\n");
    }
    hv_return_t r = hv_vm_create(config);
    if (r != HV_SUCCESS) {
        printf("  vm_create ret %#x\n", r);
        return 1;
    }
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *exit;
    hv_vcpu_config_t cpu_config = hv_vcpu_config_create();
    uint64_t feature = 0;
    r = hv_vcpu_config_get_feature_reg(cpu_config, HV_FEATURE_REG_ID_AA64PFR0_EL1, &feature);
    printf("  config PFR0        %#018llx EL2 %llu (ret %#x)\n",
           (unsigned long long)feature, (unsigned long long)((feature >> 8) & 0xf), r);
    r = hv_vcpu_config_get_feature_reg(cpu_config, HV_FEATURE_REG_ID_AA64MMFR1_EL1, &feature);
    printf("  config MMFR1       %#018llx VH %llu (ret %#x)\n",
           (unsigned long long)feature, (unsigned long long)((feature >> 8) & 0xf), r);
    r = hv_vcpu_create(&vcpu, &exit, cpu_config);
    os_release(cpu_config);
    if (r != HV_SUCCESS) {
        printf("  vcpu_create ret %#x\n", r);
        return 1;
    }
    uint64_t mmfr1 = 0;
    hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_ID_AA64MMFR1_EL1, &mmfr1);
    show("ID_AA64MMFR1_EL1", vcpu, HV_SYS_REG_ID_AA64MMFR1_EL1);
    printf("  VH                 %llu\n", (unsigned long long)((mmfr1 >> 8) & 0xf));
    show("ID_AA64MMFR2_EL1", vcpu, HV_SYS_REG_ID_AA64MMFR2_EL1);
    // ID_AA64MMFR4_EL1 (op0 3, op1 0, CRn 0, CRm 7, op2 4), which the header does not name.
    show("ID_AA64MMFR4_EL1", vcpu, (hv_sys_reg_t)0xc03c);
    show("ID_AA64PFR0_EL1", vcpu, HV_SYS_REG_ID_AA64PFR0_EL1);
    if (el2) {
        uint64_t pfr = 0, back_pfr = 0;
        hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_ID_AA64PFR0_EL1, &pfr);
        r = hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_ID_AA64PFR0_EL1, (pfr & ~0xf00ULL) | 0x100);
        hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_ID_AA64PFR0_EL1, &back_pfr);
        printf("  PFR0 EL2=1         set ret %#x, read back %#018llx: EL2 %llu\n", r,
               (unsigned long long)back_pfr, (unsigned long long)((back_pfr >> 8) & 0xf));
        hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_ID_AA64PFR0_EL1, pfr);
        uint64_t hcr = 0, back = 0;
        hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_HCR_EL2, &hcr);
        show("HCR_EL2", vcpu, HV_SYS_REG_HCR_EL2);
        r = hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_HCR_EL2, hcr | (1ULL << 34));
        hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_HCR_EL2, &back);
        printf("  HCR_EL2 | E2H      set ret %#x, read back %#018llx: E2H %llu\n", r,
               (unsigned long long)back, (unsigned long long)((back >> 34) & 1));
        r = hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_HCR_EL2, hcr & ~(1ULL << 34));
        hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_HCR_EL2, &back);
        printf("  HCR_EL2 & ~E2H     set ret %#x, read back %#018llx: E2H %llu\n", r,
               (unsigned long long)back, (unsigned long long)((back >> 34) & 1));
    }
    hv_vcpu_destroy(vcpu);
    hv_vm_destroy();
    return 0;
}

int main(void) {
    bool supported = false;
    hv_return_t r = hv_vm_config_get_el2_supported(&supported);
    printf("el2 supported: %d (ret %#x)\n", supported, r);
    int failed = one(false);
    if (supported) {
        failed |= one(true);
    }
    return failed;
}

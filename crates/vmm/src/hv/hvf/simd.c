// Stable Rust cannot pass SIMD vectors by value across FFI (the simd_ffi feature is
// unstable), and hv_vcpu_set_simd_fp_reg takes a 128-bit vector in a SIMD register
// (AAPCS64). This shim lets the C compiler produce that call from 16 bytes in memory.
#include <Hypervisor/Hypervisor.h>
#include <string.h>

hv_return_t shards_hv_vcpu_set_simd_fp_reg(hv_vcpu_t vcpu, hv_simd_fp_reg_t reg, const uint8_t bytes[16]) {
    hv_simd_fp_uchar16_t value;
    memcpy(&value, bytes, sizeof value);
    return hv_vcpu_set_simd_fp_reg(vcpu, reg, value);
}

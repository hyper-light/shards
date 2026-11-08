// Stable Rust cannot pass SIMD vectors by value across FFI (the simd_ffi feature is
// unstable), and hv_vcpu_set_simd_fp_reg takes a 128-bit vector in a SIMD register
// (AAPCS64). This shim lets the C compiler produce that call from 16 bytes in memory.
// It calls the function through the pointer the framework was asked for (ffi.rs), not
// by name: a call by name would link the framework, which dyld then loads at every start.
#include <Hypervisor/Hypervisor.h>
#include <string.h>

typedef hv_return_t (*set_simd_fp_reg_t)(hv_vcpu_t, hv_simd_fp_reg_t, hv_simd_fp_uchar16_t);

hv_return_t shards_hv_call_set_simd_fp_reg(set_simd_fp_reg_t set, hv_vcpu_t vcpu, hv_simd_fp_reg_t reg,
                                           const uint8_t bytes[16]) {
    hv_simd_fp_uchar16_t value;
    memcpy(&value, bytes, sizeof value);
    return set(vcpu, reg, value);
}

/* x86-64-v4's own: AVX-512F (vpaddd zmm, a mask register). Exit 0 once it ran. */
int main(void) {
    __asm__ volatile("vpxord %%zmm0, %%zmm0, %%zmm0\n\tvpaddd %%zmm0, %%zmm0, %%zmm1\n\tkxorw %%k1, %%k1, %%k1" ::: "xmm0", "xmm1");
    return 0;
}

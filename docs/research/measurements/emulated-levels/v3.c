/* x86-64-v3's own instructions: AVX2 (vpaddd ymm), FMA (vfmadd231ps), BMI2 (shlx),
   LZCNT, MOVBE. Exit 0 once all ran. */
int main(void) {
    unsigned a = 3, b = 0;
    __asm__ volatile("vpxor %%ymm0, %%ymm0, %%ymm0\n\tvpaddd %%ymm0, %%ymm0, %%ymm1" ::: "xmm0", "xmm1");
    __asm__ volatile("vfmadd231ps %%xmm0, %%xmm0, %%xmm1" ::: "xmm0", "xmm1");
    __asm__ volatile("shlx %1, %1, %0" : "=r"(b) : "r"(a));
    __asm__ volatile("lzcnt %1, %0" : "=r"(b) : "r"(a));
    __asm__ volatile("movbe %1, %0" : "=r"(b) : "m"(a));
    return b == 12345 ? 1 : 0;
}

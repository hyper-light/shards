// hvfbench — ground-truth microbenchmarks of Apple Hypervisor.framework (arm64).
//
// Each test runs a tiny bare-metal AArch64 guest (assembled into this binary and
// copied into guest RAM) and times one primitive the shards VMM depends on.
// Method and results: docs/research/platform-measurements.md.
//
//   ./build.sh && ./hvfbench               # all tests
//   ./hvfbench exits faults irq            # selected tests

#include <Hypervisor/Hypervisor.h>
#include <fcntl.h>
#include <libkern/OSCacheControl.h>
#include <mach-o/dyld.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/task_policy.h>
#include <mach/thread_policy.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

#define CHECK(x)                                                                \
    do {                                                                        \
        hv_return_t r_ = (x);                                                   \
        if (r_ != HV_SUCCESS) {                                                 \
            fprintf(stderr, "%s:%d: %s -> 0x%x\n", __FILE__, __LINE__, #x, r_); \
            exit(1);                                                            \
        }                                                                       \
    } while (0)
#define DIE(...) (fprintf(stderr, __VA_ARGS__), fputc('\n', stderr), exit(1))

// ---- time & statistics ------------------------------------------------------

static mach_timebase_info_data_t tb;
// Thread scheduling knobs under test, applied to every benchmark thread:
//   --qos=ui|in|ut|bg  QoS class;  --rt  Mach time-constraint (real-time) policy;
//   --lat0  Mach latency-QoS tier 0 (opts out of timer coalescing).
static qos_class_t g_qos = QOS_CLASS_UNSPECIFIED;
static bool g_rt, g_lat0;

// Host sleep-until-deadline primitives (absolute mach time) under test.
enum { SLEEP_MWU, SLEEP_KQ, SLEEP_NS, SLEEP_COUNT };
static const char *const sleep_name[SLEEP_COUNT] = {"mach_wait_until", "kevent NOTE_CRITICAL leeway=0", "nanosleep"};

static void sleep_until(int api, uint64_t deadline) {
    uint64_t now = mach_absolute_time();
    if (deadline <= now) return;
    switch (api) {
    case SLEEP_MWU: mach_wait_until(deadline); break;
    case SLEEP_KQ: {
        static _Thread_local int kq = -1;
        if (kq < 0) kq = kqueue();
        struct kevent64_s kev;
        EV_SET64(&kev, 1, EVFILT_TIMER, EV_ADD | EV_ONESHOT,
                 NOTE_MACHTIME | NOTE_ABSOLUTE | NOTE_CRITICAL | NOTE_LEEWAY, deadline, 0, 0, 0);
        struct kevent64_s out;
        kevent64(kq, &kev, 1, &out, 1, 0, NULL);
        break;
    }
    case SLEEP_NS: {
        uint64_t ns = (deadline - now) * tb.numer / tb.denom;
        struct timespec ts = {(time_t)(ns / 1000000000), (long)(ns % 1000000000)};
        nanosleep(&ts, NULL);
        break;
    }
    }
}
static int g_sleep = SLEEP_MWU;

static void apply_qos(void) {
    if (g_qos != QOS_CLASS_UNSPECIFIED) pthread_set_qos_class_self_np(g_qos, 0);
    thread_act_t self = pthread_mach_thread_np(pthread_self());
    if (g_lat0) {
        thread_latency_qos_policy_data_t lp = {LATENCY_QOS_TIER_0};
        if (thread_policy_set(self, THREAD_LATENCY_QOS_POLICY, (thread_policy_t)&lp, THREAD_LATENCY_QOS_POLICY_COUNT))
            fprintf(stderr, "latency qos policy failed\n");
    }
    if (g_rt) {
        double per_ms = 1e6 * tb.denom / tb.numer;  // absolute-time ticks per ms
        thread_time_constraint_policy_data_t p = {
            .period = 0, .computation = (uint32_t)(0.5 * per_ms), .constraint = (uint32_t)per_ms, .preemptible = 1};
        if (thread_policy_set(self, THREAD_TIME_CONSTRAINT_POLICY, (thread_policy_t)&p,
                              THREAD_TIME_CONSTRAINT_POLICY_COUNT))
            fprintf(stderr, "time-constraint policy failed\n");
    }
}
static inline uint64_t ticks(void) { return mach_absolute_time(); }
static inline double tns(uint64_t t) { return (double)t * tb.numer / tb.denom; }

typedef struct { double *v; size_t n, cap; } samples_t;

static void push(samples_t *s, double x) {
    if (s->n == s->cap) {
        s->cap = s->cap ? 2 * s->cap : 1024;
        s->v = realloc(s->v, s->cap * sizeof *s->v);
    }
    s->v[s->n++] = x;
}

static int cmpd(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

// Prints order statistics (divided by scale) and frees the samples.
static void report(const char *name, samples_t *s, const char *unit, double scale) {
    if (!s->n) { printf("  %-46s (no samples)\n", name); return; }
    qsort(s->v, s->n, sizeof *s->v, cmpd);
    double sum = 0;
    for (size_t i = 0; i < s->n; i++) sum += s->v[i];
#define Q(q) (s->v[(size_t)((q) * (double)(s->n - 1))] / scale)
    printf("  %-46s n=%-6zu min=%-9.3f p50=%-9.3f p90=%-9.3f p99=%-9.3f max=%-10.3f mean=%.3f %s\n",
           name, s->n, Q(0.0), Q(0.5), Q(0.9), Q(0.99), Q(1.0), sum / (double)s->n / scale, unit);
#undef Q
    free(s->v);
    memset(s, 0, sizeof *s);
}

static void rand_sleep_us(unsigned lo, unsigned hi) { usleep(lo + arc4random_uniform(hi - lo + 1)); }

// ---- guest programs (AArch64, position independent) ------------------------

#define GUEST(name, body)                                                       \
    __asm__(".text\n.p2align 2\n.globl _g_" #name "\n.globl _g_" #name "_end\n" \
            "_g_" #name ":\n" body "\n_g_" #name "_end:\n");                    \
    extern const uint8_t g_##name[], g_##name##_end[];

GUEST(hvc, "1: hvc #0\n b 1b")
GUEST(mmio_w, "1: str w2, [x1]\n b 1b")
GUEST(mmio_r, "1: ldr w2, [x1]\n b 1b")
GUEST(wfi, "1: wfi\n hvc #1\n b 1b")
GUEST(idregs,
      "mrs x0, cntfrq_el0\n mrs x1, midr_el1\n mrs x2, mpidr_el1\n mrs x3, id_aa64mmfr0_el1\n"
      "mrs x4, id_aa64pfr0_el1\n mrs x5, id_aa64pfr1_el1\n mrs x6, ctr_el0\n mrs x7, id_aa64isar0_el1\n"
      "mrs x8, id_aa64dfr0_el1\n mrs x9, cntvct_el0\n hvc #0\n1: b 1b")
// x0 = base, x1 = count, x2 = stride, x3 = 1 write / 0 read; hvc #0 when done.
GUEST(touch,
      "cbz x3, 3f\n"
      "1: str xzr, [x0]\n add x0, x0, x2\n subs x1, x1, #1\n b.ne 1b\n hvc #0\n2: b 2b\n"
      "3: ldr x4, [x0]\n add x0, x0, x2\n subs x1, x1, #1\n b.ne 3b\n hvc #0\n4: b 4b")
// GICv3 bring-up. x0 = GICD, x1 = this CPU's GICR RD_base, x2 = SPI INTID (0: none),
// x3 = vtimer period in ticks (0: none), x4 = idle mode: 0 busy-spin, 1 WFI, 2 hvc #4
// (paravirt idle: the VMM sleeps until the armed vtimer deadline). hvc #1 = ready.
GUEST(gic,
      "mov w9, #0x13\n str w9, [x0]\n"                                  // GICD_CTLR: Grp0|Grp1|ARE
      "ldr w9, [x1, #0x14]\n bic w9, w9, #2\n str w9, [x1, #0x14]\n"   // GICR_WAKER.ProcessorSleep=0
      "1: ldr w9, [x1, #0x14]\n tbnz w9, #2, 1b\n"                     // wait ChildrenAsleep=0
      "cbz x2, 5f\n"
      "lsr x10, x2, #5\n and x11, x2, #31\n mov w12, #1\n lsl w12, w12, w11\n"
      "add x13, x0, #0x80\n ldr w14, [x13, x10, lsl #2]\n orr w14, w14, w12\n str w14, [x13, x10, lsl #2]\n"
      "lsr x15, x2, #4\n and x16, x2, #15\n lsl x16, x16, #1\n add x16, x16, #1\n mov w17, #1\n lsl w17, w17, w16\n"
      "add x13, x0, #0xc00\n ldr w14, [x13, x15, lsl #2]\n orr w14, w14, w17\n str w14, [x13, x15, lsl #2]\n"
      "add x13, x0, #0x400\n mov w14, #0x80\n strb w14, [x13, x2]\n"   // IPRIORITYR
      "add x13, x0, #0x6000\n str xzr, [x13, x2, lsl #3]\n"            // IROUTER -> aff 0
      "add x13, x0, #0x100\n str w12, [x13, x10, lsl #2]\n"            // ISENABLER
      "5: cbz x3, 6f\n"
      "add x12, x1, #0x10, lsl #12\n mov w13, #0x8000000\n"            // SGI_base; PPI 27
      "ldr w14, [x12, #0x80]\n orr w14, w14, w13\n str w14, [x12, #0x80]\n str w13, [x12, #0x100]\n"
      "6: mrs x9, S3_0_C12_C12_5\n orr x9, x9, #1\n msr S3_0_C12_C12_5, x9\n isb\n" // ICC_SRE_EL1.SRE
      "mov x9, #0xff\n msr S3_0_C4_C6_0, x9\n"                          // ICC_PMR_EL1
      "mov x9, #1\n msr S3_0_C12_C12_7, x9\n isb\n"                     // ICC_IGRPEN1_EL1
      "msr daifclr, #2\n hvc #1\n"
      "cbz x3, 7f\n mrs x5, cntvct_el0\n add x6, x5, x3\n msr cntv_cval_el0, x6\n"
      "mov x7, #1\n msr cntv_ctl_el0, x7\n isb\n"
      "7: cbz x4, 8f\n cmp x4, #2\n b.eq 9f\n wfi\n b 7b\n"
      "9: hvc #4\n b 7b\n"                                                // paravirt idle hint
      "8: b 8b")
// IRQ vector (VBAR+0x280). Timer: report lateness (x8, ticks) via hvc #3, re-arm.
// SPI: EOI, report via hvc #2.
GUEST(irq_vec,
      "mrs x9, S3_0_C12_C12_0\n cmp x9, #27\n b.ne 1f\n"
      "mrs x8, cntvct_el0\n sub x8, x8, x6\n msr cntv_ctl_el0, xzr\n msr S3_0_C12_C12_1, x9\n hvc #3\n"
      "mrs x5, cntvct_el0\n add x6, x5, x3\n msr cntv_cval_el0, x6\n mov x7, #1\n msr cntv_ctl_el0, x7\n isb\n eret\n"
      "1: msr S3_0_C12_C12_1, x9\n hvc #2\n eret")
// Synchronous-exception vector (VBAR+0x200): report unexpected guest faults.
GUEST(sync_vec, "mrs x9, esr_el1\n mrs x10, elr_el1\n mrs x11, far_el1\n hvc #0xe\n1: b 1b")

// ---- guest layout ------------------------------------------------------------

#define SYS_GPA 0x80000000ull   // 2 MiB system region: code, page table, vectors, stack
#define SYS_SIZE (2ull << 20)
#define CODE_OFF 0x0000
#define PT_OFF 0x4000            // L1 table, 4 KiB granule, 1 GiB blocks
#define VEC_OFF 0x8000
#define STACK_OFF 0x10000
#define DATA_GPA 0x100000000ull  // region under test (and child-process RAM)
#define MMIO_GPA 0x40000000ull   // unmapped: every access is a data-abort exit
#define GICD_GPA 0x08000000ull

#define EC(e) ((uint32_t)(((e)->exception.syndrome >> 26) & 0x3f))
#define ISS_IMM16(e) ((uint32_t)((e)->exception.syndrome & 0xffff))
enum { EC_WFX = 0x01, EC_HVC = 0x16, EC_DABT = 0x24 };

typedef struct {
    uint8_t *sys;
    hv_ipa_t gicd, gicr;  // gicr: vCPU 0's redistributor; 0 when no GIC
    hv_vcpu_t vcpu;
    hv_vcpu_exit_t *exit;
} vm_t;

static void gic_create(hv_ipa_t *gicd) {
    size_t dsz, ral;
    CHECK(hv_gic_get_distributor_size(&dsz));
    CHECK(hv_gic_get_redistributor_base_alignment(&ral));
    hv_ipa_t rbase = (GICD_GPA + dsz + ral - 1) & ~(hv_ipa_t)(ral - 1);
    hv_gic_config_t gc = hv_gic_config_create();
    CHECK(hv_gic_config_set_distributor_base(gc, GICD_GPA));
    CHECK(hv_gic_config_set_redistributor_base(gc, rbase));
    CHECK(hv_gic_create(gc));
    os_release(gc);
    *gicd = GICD_GPA;
}

static void vm_create(vm_t *vm, bool gran4k, bool gic) {
    memset(vm, 0, sizeof *vm);
    hv_vm_config_t cfg = hv_vm_config_create();
    if (gran4k) CHECK(hv_vm_config_set_ipa_granule(cfg, HV_IPA_GRANULE_4KB));
    CHECK(hv_vm_create(cfg));
    os_release(cfg);

    vm->sys = mmap(NULL, SYS_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (vm->sys == MAP_FAILED) DIE("mmap sys");
    CHECK(hv_vm_map(vm->sys, SYS_GPA, SYS_SIZE, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));

    // Identity map: [0,2G) device (GIC + MMIO), [2G,3G) normal (sys), [4G,16G) normal (data).
    uint64_t *l1 = (uint64_t *)(vm->sys + PT_OFF);
    const uint64_t blk = 1, af = 1ull << 10, ish = 3ull << 8, dev = 1ull << 2;
    l1[0] = 0x00000000ull | af | dev | blk;
    l1[1] = 0x40000000ull | af | dev | blk;
    l1[2] = 0x80000000ull | af | ish | blk;
    for (uint64_t i = 4; i < 16; i++) l1[i] = (i << 30) | af | ish | blk;

    memcpy(vm->sys + VEC_OFF + 0x200, g_sync_vec, (size_t)(g_sync_vec_end - g_sync_vec));
    memcpy(vm->sys + VEC_OFF + 0x280, g_irq_vec, (size_t)(g_irq_vec_end - g_irq_vec));
    sys_icache_invalidate(vm->sys + VEC_OFF, 0x800);
    if (gic) gic_create(&vm->gicd);
}

static void vm_destroy(vm_t *vm) {
    CHECK(hv_vm_destroy());
    munmap(vm->sys, SYS_SIZE);
}

static void load_code(vm_t *vm, const uint8_t *start, const uint8_t *end) {
    memcpy(vm->sys + CODE_OFF, start, (size_t)(end - start));
    sys_icache_invalidate(vm->sys + CODE_OFF, (size_t)(end - start));
}

// Must run on the thread that will run the vCPU.
static void vcpu_create(vm_t *vm) {
    apply_qos();
    CHECK(hv_vcpu_create(&vm->vcpu, &vm->exit, NULL));
    hv_vcpu_t v = vm->vcpu;
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_MPIDR_EL1, 0));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_MAIR_EL1, 0x04ff));  // attr0 normal WB, attr1 device-nGnRE
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_TCR_EL1,             // T0SZ=25, WBWA, ISH, 4K, EPD1, IPS=36b
                              25 | (1u << 8) | (1u << 10) | (3u << 12) | (25u << 16) | (1u << 23) | (1ull << 32)));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_TTBR0_EL1, SYS_GPA + PT_OFF));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_VBAR_EL1, SYS_GPA + VEC_OFF));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_SP_EL1, SYS_GPA + STACK_OFF));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_SCTLR_EL1, 0x30d01805));  // M|C|I + RES1
    if (vm->gicd) CHECK(hv_gic_get_redistributor_base(v, &vm->gicr));
}

static void vcpu_reset(vm_t *vm, uint64_t x0, uint64_t x1, uint64_t x2, uint64_t x3, uint64_t x4) {
    hv_vcpu_t v = vm->vcpu;
    CHECK(hv_vcpu_set_reg(v, HV_REG_CPSR, 0x3c5));  // EL1h, DAIF masked
    CHECK(hv_vcpu_set_reg(v, HV_REG_PC, SYS_GPA + CODE_OFF));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X0, x0));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X1, x1));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X2, x2));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X3, x3));
    CHECK(hv_vcpu_set_reg(v, HV_REG_X4, x4));
}

static void die_exit(vm_t *vm, const char *what) {
    hv_vcpu_exit_t *e = vm->exit;
    uint64_t pc = 0, x9 = 0, x10 = 0, x11 = 0;
    hv_vcpu_get_reg(vm->vcpu, HV_REG_PC, &pc);
    hv_vcpu_get_reg(vm->vcpu, HV_REG_X9, &x9);
    hv_vcpu_get_reg(vm->vcpu, HV_REG_X10, &x10);
    hv_vcpu_get_reg(vm->vcpu, HV_REG_X11, &x11);
    DIE("%s: unexpected exit reason=%u esr=0x%llx va=0x%llx ipa=0x%llx pc=0x%llx "
        "(guest esr_el1=0x%llx elr=0x%llx far=0x%llx)",
        what, e->reason, e->exception.syndrome, e->exception.virtual_address,
        e->exception.physical_address, pc, x9, x10, x11);
}

// Runs until an HVC exit and returns its immediate; any other exit is fatal.
static uint32_t run_to_hvc(vm_t *vm, const char *what) {
    CHECK(hv_vcpu_run(vm->vcpu));
    if (vm->exit->reason != HV_EXIT_REASON_EXCEPTION || EC(vm->exit) != EC_HVC) die_exit(vm, what);
    uint32_t imm = ISS_IMM16(vm->exit);
    if (imm == 0xe) die_exit(vm, what);
    return imm;
}

// ---- info, lifecycle, mapping --------------------------------------------------

static void t_info(void) {
    uint32_t maxv = 0, ipa_def = 0, ipa_max = 0, spi_base, spi_count, vt;
    bool el2 = false;
    hv_ipa_granule_t gran;
    size_t dsz, dal, rrs, rsz, ral, msz, mal;
    CHECK(hv_vm_get_max_vcpu_count(&maxv));
    CHECK(hv_vm_config_get_default_ipa_size(&ipa_def));
    CHECK(hv_vm_config_get_max_ipa_size(&ipa_max));
    CHECK(hv_vm_config_get_el2_supported(&el2));
    CHECK(hv_vm_config_get_default_ipa_granule(&gran));
    CHECK(hv_gic_get_distributor_size(&dsz));
    CHECK(hv_gic_get_distributor_base_alignment(&dal));
    CHECK(hv_gic_get_redistributor_region_size(&rrs));
    CHECK(hv_gic_get_redistributor_size(&rsz));
    CHECK(hv_gic_get_redistributor_base_alignment(&ral));
    CHECK(hv_gic_get_msi_region_size(&msz));
    CHECK(hv_gic_get_msi_region_base_alignment(&mal));
    CHECK(hv_gic_get_spi_interrupt_range(&spi_base, &spi_count));
    CHECK(hv_gic_get_intid(HV_GIC_INT_EL1_VIRTUAL_TIMER, &vt));
    uint64_t frq;
    __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(frq));
    printf("info\n  max_vcpus=%u ipa_default=%u ipa_max=%u el2_supported=%d default_granule=%s "
           "cntfrq=%llu host_page=%ld\n",
           maxv, ipa_def, ipa_max, el2, gran == HV_IPA_GRANULE_4KB ? "4K" : "16K", frq, sysconf(_SC_PAGESIZE));
    printf("  gic: dist size=0x%zx align=0x%zx; redist region=0x%zx per-cpu=0x%zx align=0x%zx; "
           "msi size=0x%zx align=0x%zx; spi base=%u count=%u; vtimer intid=%u\n",
           dsz, dal, rrs, rsz, ral, msz, mal, spi_base, spi_count, vt);
}

static void *vcpu_lifecycle_thread(void *arg) {
    (void)arg;
    samples_t c = {0}, d = {0};
    for (int i = 0; i < 200; i++) {
        hv_vcpu_t v;
        hv_vcpu_exit_t *e;
        uint64_t t0 = ticks();
        CHECK(hv_vcpu_create(&v, &e, NULL));
        uint64_t t1 = ticks();
        CHECK(hv_vcpu_destroy(v));
        push(&c, tns(t1 - t0));
        push(&d, tns(ticks() - t1));
    }
    report("hv_vcpu_create", &c, "us", 1e3);
    report("hv_vcpu_destroy", &d, "us", 1e3);
    return NULL;
}

static void t_lifecycle(void) {
    printf("lifecycle\n");
    samples_t c = {0}, d = {0}, g = {0};
    for (int i = 0; i < 200; i++) {
        uint64_t t0 = ticks();
        CHECK(hv_vm_create(NULL));
        uint64_t t1 = ticks();
        CHECK(hv_vm_destroy());
        push(&c, tns(t1 - t0));
        push(&d, tns(ticks() - t1));
    }
    report("hv_vm_create(NULL)", &c, "us", 1e3);
    report("hv_vm_destroy", &d, "us", 1e3);
    for (int i = 0; i < 200; i++) {
        CHECK(hv_vm_create(NULL));
        hv_ipa_t gicd;
        uint64_t t0 = ticks();
        gic_create(&gicd);
        push(&g, tns(ticks() - t0));
        CHECK(hv_vm_destroy());
    }
    report("hv_gic_create (incl. config)", &g, "us", 1e3);
    CHECK(hv_vm_create(NULL));
    pthread_t th;
    pthread_create(&th, NULL, vcpu_lifecycle_thread, NULL);
    pthread_join(th, NULL);
    CHECK(hv_vm_destroy());
}

static void t_map(void) {
    printf("map\n");
    CHECK(hv_vm_create(NULL));
    const size_t sizes[] = {16ull << 20, 256ull << 20, 1ull << 30, 8ull << 30};
    for (size_t k = 0; k < sizeof sizes / sizeof *sizes; k++) {
        size_t sz = sizes[k];
        samples_t m = {0}, u = {0}, p = {0};
        for (int i = 0; i < 20; i++) {
            void *mem = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0);
            if (mem == MAP_FAILED) DIE("mmap %zu", sz);
            uint64_t t0 = ticks();
            CHECK(hv_vm_map(mem, DATA_GPA, sz, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
            uint64_t t1 = ticks();
            CHECK(hv_vm_protect(DATA_GPA, sz, HV_MEMORY_READ | HV_MEMORY_EXEC));
            uint64_t t2 = ticks();
            CHECK(hv_vm_unmap(DATA_GPA, sz));
            uint64_t t3 = ticks();
            push(&m, tns(t1 - t0));
            push(&p, tns(t2 - t1));
            push(&u, tns(t3 - t2));
            munmap(mem, sz);
        }
        char name[64];
        snprintf(name, sizeof name, "hv_vm_map %zu MiB (untouched anon)", sz >> 20);
        report(name, &m, "us", 1e3);
        snprintf(name, sizeof name, "hv_vm_protect %zu MiB -> RX", sz >> 20);
        report(name, &p, "us", 1e3);
        snprintf(name, sizeof name, "hv_vm_unmap %zu MiB", sz >> 20);
        report(name, &u, "us", 1e3);
    }
    CHECK(hv_vm_destroy());

    // Sub-host-page mapping with a 4 KiB IPA granule (host pages are 16 KiB).
    hv_vm_config_t cfg = hv_vm_config_create();
    CHECK(hv_vm_config_set_ipa_granule(cfg, HV_IPA_GRANULE_4KB));
    CHECK(hv_vm_create(cfg));
    os_release(cfg);
    uint8_t *mem = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    hv_return_t r1 = hv_vm_map(mem + 4096, DATA_GPA + 4096, 4096, HV_MEMORY_READ | HV_MEMORY_WRITE);
    hv_return_t r2 = hv_vm_map(mem + 16384, DATA_GPA + 8192, 4096, HV_MEMORY_READ | HV_MEMORY_WRITE);
    hv_return_t r3 = hv_vm_protect(DATA_GPA + 8192, 4096, HV_MEMORY_READ);
    printf("  4K granule: map(host+4K -> ipa+4K, 4K)=0x%x  map(host 16K-aligned -> ipa+8K, 4K)=0x%x  "
           "protect(4K)=0x%x  (0 = success)\n", r1, r2, r3);
    CHECK(hv_vm_destroy());
    munmap(mem, 1 << 20);
}

// ---- exit round trips ------------------------------------------------------------

static void *exits_thread(void *arg) {
    vm_t *vm = arg;
    const int N = 200000, WARM = 1000;
    vcpu_create(vm);

    load_code(vm, g_hvc, g_hvc_end);
    vcpu_reset(vm, 0, 0, 0, 0, 0);
    run_to_hvc(vm, "hvc");
    uint64_t pc0;
    CHECK(hv_vcpu_get_reg(vm->vcpu, HV_REG_PC, &pc0));
    printf("  hvc exit: PC = code+0x%llx (hvc at +0x0) -> PC %s advanced by hardware\n",
           pc0 - SYS_GPA - CODE_OFF, pc0 == SYS_GPA + CODE_OFF + 4 ? "IS" : "is NOT");
    for (int i = 0; i < WARM; i++) run_to_hvc(vm, "hvc warmup");
    samples_t s = {0};
    uint64_t t0 = ticks();
    for (int i = 0; i < N; i++) run_to_hvc(vm, "hvc");
    printf("  %-46s aggregate %.1f ns/exit\n", "hvc round trip", tns(ticks() - t0) / N);
    for (int i = 0; i < N; i++) {
        uint64_t a = ticks();
        run_to_hvc(vm, "hvc");
        push(&s, tns(ticks() - a));
    }
    report("hvc round trip (per-iteration)", &s, "ns", 1);

    for (int w = 1; w >= 0; w--) {
        load_code(vm, w ? g_mmio_w : g_mmio_r, w ? g_mmio_w_end : g_mmio_r_end);
        vcpu_reset(vm, 0, MMIO_GPA, 0x1234, 0, 0);
        uint64_t start = 0;
        for (int i = 0; i < 2 * N + WARM; i++) {
            if (i == WARM) start = ticks();
            uint64_t a = ticks();
            CHECK(hv_vcpu_run(vm->vcpu));
            hv_vcpu_exit_t *e = vm->exit;
            if (e->reason != HV_EXIT_REASON_EXCEPTION || EC(e) != EC_DABT) die_exit(vm, "mmio");
            uint64_t esr = e->exception.syndrome;
            if (i == 0)
                printf("  dabt %s: esr=0x%llx ISV=%llu SAS=%llu SSE=%llu SRT=%llu SF=%llu WnR=%llu ipa=0x%llx\n",
                       w ? "write" : "read", esr, (esr >> 24) & 1, (esr >> 22) & 3, (esr >> 21) & 1,
                       (esr >> 16) & 31, (esr >> 15) & 1, (esr >> 6) & 1, e->exception.physical_address);
            uint64_t pc;
            CHECK(hv_vcpu_get_reg(vm->vcpu, HV_REG_PC, &pc));
            if (!w) CHECK(hv_vcpu_set_reg(vm->vcpu, (hv_reg_t)((esr >> 16) & 31), 0x12345678));
            CHECK(hv_vcpu_set_reg(vm->vcpu, HV_REG_PC, pc + 4));
            if (i == WARM + N - 1)
                printf("  %-46s aggregate %.1f ns/exit\n",
                       w ? "mmio write exit (+get/set PC)" : "mmio read exit (+set Rt, PC)",
                       tns(ticks() - start) / N);
            if (i >= WARM + N) push(&s, tns(ticks() - a));
        }
        report(w ? "mmio write exit (per-iteration)" : "mmio read exit (per-iteration)", &s, "ns", 1);
    }
    CHECK(hv_vcpu_destroy(vm->vcpu));
    return NULL;
}

static void t_exits(void) {
    printf("exits\n");
    vm_t vm;
    vm_create(&vm, false, false);
    pthread_t th;
    pthread_create(&th, NULL, exits_thread, &vm);
    pthread_join(th, NULL);
    vm_destroy(&vm);
}

// ---- stage-2 fault costs ------------------------------------------------------------

enum { B_ANON, B_ANON_PREFAULT, B_FILE_PRIV, B_FILE_PRIV_HOSTREAD, B_FILE_SHARED, B_COUNT };
static const char *const backing_name[B_COUNT] = {
    "anon", "anon host-prefaulted", "file MAP_PRIVATE (cache hot)",
    "file MAP_PRIVATE host-pre-read", "file MAP_SHARED (cache hot)"};

typedef struct {
    vm_t vm;
    int fd;
    size_t size;
    bool gran4k;
} faults_ctx;

static double time_touch(vm_t *vm, size_t size, size_t stride, bool write) {
    load_code(vm, g_touch, g_touch_end);
    vcpu_reset(vm, DATA_GPA, size / stride, stride, write, 0);
    uint64_t t0 = ticks();
    run_to_hvc(vm, "touch");
    return tns(ticks() - t0);
}

static void *faults_thread(void *arg) {
    faults_ctx *c = arg;
    vm_t *vm = &c->vm;
    vcpu_create(vm);
    const size_t strides[] = {16384, 4096};
    for (int b = B_ANON; b < B_COUNT; b++) {
        for (int op = 1; op >= 0; op--) {
            if (b == B_FILE_SHARED && op) continue;  // never write the shared file
            for (size_t si = 0; si < (c->gran4k ? 2u : 1u); si++) {
                size_t stride = strides[si];
                uint8_t *mem = b <= B_ANON_PREFAULT
                    ? mmap(NULL, c->size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0)
                    : mmap(NULL, c->size, PROT_READ | PROT_WRITE, b == B_FILE_SHARED ? MAP_SHARED : MAP_PRIVATE,
                           c->fd, 0);
                if (mem == MAP_FAILED) DIE("mmap backing");
                if (b == B_ANON_PREFAULT) memset(mem, 1, c->size);
                if (b == B_FILE_PRIV_HOSTREAD) {
                    volatile uint8_t sink = 0;
                    for (size_t o = 0; o < c->size; o += 16384) sink += mem[o];
                    (void)sink;
                }
                uint64_t m0 = ticks();
                CHECK(hv_vm_map(mem, DATA_GPA, c->size, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
                double map_ns = tns(ticks() - m0);
                double first = time_touch(vm, c->size, stride, op);
                double second = time_touch(vm, c->size, stride, op);
                double n = (double)(c->size / stride);
                printf("  %-32s %-5s stride=%-5zu first=%8.1f ns/page  resident=%6.1f ns/page  (map %.1f us)\n",
                       backing_name[b], op ? "write" : "read", stride, first / n, second / n, map_ns / 1e3);
                CHECK(hv_vm_unmap(DATA_GPA, c->size));
                munmap(mem, c->size);
            }
        }
    }

    // Write-protect dirty tracking: guest write -> permission-fault exit -> unprotect page -> resume.
    size_t stride = c->gran4k ? 4096 : 16384;
    uint8_t *mem = mmap(NULL, c->size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0);
    memset(mem, 1, c->size);
    CHECK(hv_vm_map(mem, DATA_GPA, c->size, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
    time_touch(vm, c->size, stride, true);  // make stage-2 mappings resident
    uint64_t p0 = ticks();
    CHECK(hv_vm_protect(DATA_GPA, c->size, HV_MEMORY_READ | HV_MEMORY_EXEC));
    double prot_ns = tns(ticks() - p0);
    load_code(vm, g_touch, g_touch_end);
    size_t faults = 0;
    vcpu_reset(vm, DATA_GPA, c->size / stride, stride, 1, 0);
    uint64_t t0 = ticks();
    for (;;) {
        CHECK(hv_vcpu_run(vm->vcpu));
        hv_vcpu_exit_t *e = vm->exit;
        if (e->reason == HV_EXIT_REASON_EXCEPTION && EC(e) == EC_HVC) break;
        if (e->reason != HV_EXIT_REASON_EXCEPTION || EC(e) != EC_DABT) die_exit(vm, "dirty");
        hv_ipa_t page = e->exception.physical_address & ~(hv_ipa_t)(stride - 1);
        CHECK(hv_vm_protect(page, stride, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
        faults++;  // PC not advanced: the faulting store re-executes
    }
    double total = tns(ticks() - t0);
    printf("  write-protect dirty tracking: protect %zu MiB = %.1f us; %zu faults; %.1f ns per dirtied %zu-byte page\n",
           c->size >> 20, prot_ns / 1e3, faults, total / (double)faults, stride);
    CHECK(hv_vm_unmap(DATA_GPA, c->size));
    munmap(mem, c->size);
    CHECK(hv_vcpu_destroy(vm->vcpu));
    return NULL;
}

static void t_faults(void) {
    const size_t size = 512ull << 20;
    char path[1024];
    const char *tmp = getenv("TMPDIR");
    snprintf(path, sizeof path, "%s/hvfbench-backing-XXXXXX", tmp ? tmp : "/tmp");
    int fd = mkstemp(path);
    if (fd < 0) DIE("mkstemp");
    unlink(path);
    uint8_t *buf = malloc(1 << 20);
    for (size_t i = 0; i < (1 << 20); i++) buf[i] = (uint8_t)(i * 131 + 7);
    for (size_t o = 0; o < size; o += 1 << 20)
        if (write(fd, buf, 1 << 20) != 1 << 20) DIE("write backing");
    free(buf);
    uint8_t *warm = mmap(NULL, size, PROT_READ, MAP_SHARED, fd, 0);  // warm the page cache
    volatile uint8_t sink = 0;
    for (size_t o = 0; o < size; o += 16384) sink += warm[o];
    (void)sink;
    munmap(warm, size);

    for (int g = 0; g < 2; g++) {
        printf("faults (ipa granule %s, %zu MiB region)\n", g ? "4K" : "16K", size >> 20);
        faults_ctx c = {.fd = fd, .size = size, .gran4k = g};
        vm_create(&c.vm, g, false);
        pthread_t th;
        pthread_create(&th, NULL, faults_thread, &c);
        pthread_join(th, NULL);
        vm_destroy(&c.vm);
    }
    close(fd);
}

// Parallel stage-2 fault throughput: K vCPUs (one thread each) read disjoint slices of a
// cache-hot MAP_PRIVATE file mapping. Answers whether helper vCPUs can prefetch a restored
// snapshot's working set faster than one vCPU faulting alone.
typedef struct {
    vm_t *vm;
    size_t slice, stride;
    int idx;
    _Atomic int *ready, *go;  // start gate: all vCPUs created before any starts faulting
    double ns;
} pf_arg;

static void *pfault_thread(void *arg) {
    pf_arg *a = arg;
    vm_t v = *a->vm;  // private copy: own vcpu/exit, shared RAM
    vcpu_create(&v);
    vcpu_reset(&v, DATA_GPA + (uint64_t)a->idx * a->slice, a->slice / a->stride, a->stride, 0, 0);
    atomic_fetch_add(a->ready, 1);
    while (!atomic_load(a->go)) {}
    uint64_t t0 = ticks();
    run_to_hvc(&v, "pfault");
    a->ns = tns(ticks() - t0);
    CHECK(hv_vcpu_destroy(v.vcpu));
    return NULL;
}

static void t_pfault(void) {
    const size_t size = 1ull << 30, stride = 16384;
    printf("pfault (K vCPUs fault disjoint slices of a %zu MiB cache-hot MAP_PRIVATE file, 16K granule)\n", size >> 20);
    char path[1024];
    const char *tmp = getenv("TMPDIR");
    snprintf(path, sizeof path, "%s/hvfbench-pf-XXXXXX", tmp ? tmp : "/tmp");
    int fd = mkstemp(path);
    if (fd < 0) DIE("mkstemp");
    unlink(path);
    if (ftruncate(fd, (off_t)size)) DIE("ftruncate");
    uint8_t *w = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    for (size_t o = 0; o < size; o += stride) w[o] = (uint8_t)o;  // materialize + warm the cache
    munmap(w, size);
    const int ks[] = {1, 2, 4, 8, 12};
    for (size_t ki = 0; ki < sizeof ks / sizeof *ks; ki++) {
        int k = ks[ki];
        vm_t vm;
        vm_create(&vm, false, false);
        load_code(&vm, g_touch, g_touch_end);
        uint8_t *mem = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
        CHECK(hv_vm_map(mem, DATA_GPA, size, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
        _Atomic int ready = 0, go = 0;
        pthread_t th[16];
        pf_arg args[16];
        for (int i = 0; i < k; i++) {
            args[i] = (pf_arg){.vm = &vm, .slice = size / (size_t)k, .stride = stride, .idx = i, .ready = &ready, .go = &go};
            pthread_create(&th[i], NULL, pfault_thread, &args[i]);
        }
        while (atomic_load(&ready) < k) {}
        atomic_store(&go, 1);
        double worst = 0;
        for (int i = 0; i < k; i++) {
            pthread_join(th[i], NULL);
            if (args[i].ns > worst) worst = args[i].ns;
        }
        double pages = (double)(size / stride);
        printf("  K=%-2d wall=%7.2f ms  %6.0f ns/page effective  %5.2f GB/s mapped-in\n", k, worst / 1e6,
               worst / pages, (double)size / worst);
        CHECK(hv_vm_unmap(DATA_GPA, size));
        munmap(mem, size);
        vm_destroy(&vm);
    }
    close(fd);
}

// What the guest sees by default (resolves UNVERIFIED items in the ground-truth doc), and
// which redistributor HVF marks GICR_TYPER.Last (decides the DT redistributor extent).
static void *idregs_thread(void *arg) {
    vm_t *vm = arg;
    vcpu_create(vm);
    load_code(vm, g_idregs, g_idregs_end);
    vcpu_reset(vm, 0, 0, 0, 0, 0);
    uint64_t hv0 = ticks();
    run_to_hvc(vm, "idregs");
    static const char *const names[] = {"CNTFRQ_EL0", "MIDR_EL1", "MPIDR_EL1", "ID_AA64MMFR0_EL1", "ID_AA64PFR0_EL1",
                                        "ID_AA64PFR1_EL1", "CTR_EL0", "ID_AA64ISAR0_EL1", "ID_AA64DFR0_EL1"};
    for (int r = 0; r < 9; r++) {
        uint64_t v;
        CHECK(hv_vcpu_get_reg(vm->vcpu, (hv_reg_t)(HV_REG_X0 + r), &v));
        printf("  guest %-18s = 0x%016llx\n", names[r], v);
    }
    uint64_t cnt, off;
    CHECK(hv_vcpu_get_reg(vm->vcpu, HV_REG_X9, &cnt));
    CHECK(hv_vcpu_get_vtimer_offset(vm->vcpu, &off));
    printf("  guest CNTVCT_EL0 = host mach_absolute_time - 0x%llx (vtimer offset reads 0x%llx)\n", hv0 - cnt, off);
    uint64_t exec;
    CHECK(hv_vcpu_get_exec_time(vm->vcpu, &exec));
    printf("  hv_vcpu_get_exec_time after one short run = %llu\n", exec);
    CHECK(hv_vcpu_destroy(vm->vcpu));
    return NULL;
}

// Every vCPU is created before any reads its redistributor, then they report in index order.
typedef struct { _Atomic int created, go, turn, done; } gicr_sync;
typedef struct { gicr_sync *sync; int idx; } gicr_arg;

static void *gicr_thread(void *p) {
    gicr_arg *a = p;
    gicr_sync *s = a->sync;
    hv_vcpu_t v;
    hv_vcpu_exit_t *e;
    CHECK(hv_vcpu_create(&v, &e, NULL));
    CHECK(hv_vcpu_set_sys_reg(v, HV_SYS_REG_MPIDR_EL1, (uint64_t)a->idx));  // Aff0 = index
    atomic_fetch_add(&s->created, 1);
    while (!atomic_load(&s->go) || atomic_load(&s->turn) != a->idx) {}
    hv_ipa_t base;
    uint64_t typer;
    CHECK(hv_gic_get_redistributor_base(v, &base));
    CHECK(hv_gic_get_redistributor_reg(v, HV_GIC_REDISTRIBUTOR_REG_GICR_TYPER, &typer));
    printf("    vcpu %d: GICR base=0x%llx TYPER=0x%016llx (aff=0x%llx proc=%llu Last=%llu)\n", a->idx, base, typer,
           typer >> 32, (typer >> 8) & 0xffff, (typer >> 4) & 1);
    atomic_fetch_add(&s->turn, 1);
    while (!atomic_load(&s->done)) {}
    CHECK(hv_vcpu_destroy(v));
    return NULL;
}

static void t_guestinfo(void) {
    printf("guestinfo (default guest-visible registers; GICR_TYPER.Last placement)\n");
    vm_t vm;
    vm_create(&vm, false, true);
    pthread_t th;
    pthread_create(&th, NULL, idregs_thread, &vm);
    pthread_join(th, NULL);
    vm_destroy(&vm);
    const int counts[] = {1, 2, 4};
    for (size_t c = 0; c < 3; c++) {
        int n = counts[c];
        printf("  %d vCPU(s):\n", n);
        vm_create(&vm, false, true);
        gicr_sync sync = {0};
        pthread_t ths[4];
        gicr_arg args[4];
        for (int i = 0; i < n; i++) {
            args[i] = (gicr_arg){&sync, i};
            pthread_create(&ths[i], NULL, gicr_thread, &args[i]);
        }
        while (atomic_load(&sync.created) < n) {}
        atomic_store(&sync.go, 1);
        while (atomic_load(&sync.turn) < n) {}
        atomic_store(&sync.done, 1);
        for (int i = 0; i < n; i++) pthread_join(ths[i], NULL);
        vm_destroy(&vm);
    }
}

// ---- WFI, interrupts, kicks, vtimer ------------------------------------------------

typedef struct {
    vm_t vm;
    uint64_t x2, x3, x4;       // gic program args (SPI, timer period, idle-in-WFI)
    _Atomic int ready;         // 1: guest set up; 2: vtimer sample target reached
    _Atomic int stop;
    _Atomic uint64_t seq, t1;  // vCPU-side timestamp of the last reported event
    samples_t lateness;        // vtimer lateness
    size_t n_wfx, n_vtimer, n_cancel, n_irq, target;
} irq_ctx;

static void *irq_thread(void *arg) {
    irq_ctx *c = arg;
    vm_t *vm = &c->vm;
    vcpu_create(vm);
    load_code(vm, g_gic, g_gic_end);
    vcpu_reset(vm, vm->gicd, vm->gicr, c->x2, c->x3, c->x4);
    while (!atomic_load(&c->stop)) {
        CHECK(hv_vcpu_run(vm->vcpu));
        uint64_t now = ticks();
        hv_vcpu_exit_t *e = vm->exit;
        if (e->reason == HV_EXIT_REASON_CANCELED) {
            c->n_cancel++;
            atomic_store(&c->t1, now);
            atomic_fetch_add(&c->seq, 1);
            continue;
        }
        if (e->reason == HV_EXIT_REASON_VTIMER_ACTIVATED) { c->n_vtimer++; continue; }
        if (e->reason != HV_EXIT_REASON_EXCEPTION) die_exit(vm, "irq");
        if (EC(e) == EC_WFX) {  // WFI trapped to userspace: resume at once (busy-wait semantics)
            c->n_wfx++;
            uint64_t pc;
            CHECK(hv_vcpu_get_reg(vm->vcpu, HV_REG_PC, &pc));
            CHECK(hv_vcpu_set_reg(vm->vcpu, HV_REG_PC, pc + 4));
            continue;
        }
        if (EC(e) != EC_HVC) die_exit(vm, "irq");
        switch (ISS_IMM16(e)) {
        case 1: atomic_store(&c->ready, 1); break;
        case 2:
            c->n_irq++;
            atomic_store(&c->t1, now);
            atomic_fetch_add(&c->seq, 1);
            break;
        case 4: {  // paravirt idle: sleep until the guest's armed vtimer deadline, then re-enter
            uint64_t ctl, cval, off;
            CHECK(hv_vcpu_get_sys_reg(vm->vcpu, HV_SYS_REG_CNTV_CTL_EL0, &ctl));
            CHECK(hv_vcpu_get_sys_reg(vm->vcpu, HV_SYS_REG_CNTV_CVAL_EL0, &cval));
            CHECK(hv_vcpu_get_vtimer_offset(vm->vcpu, &off));
            if ((ctl & 3) == 1) sleep_until(g_sleep, cval + off);  // enabled, unmasked
            break;
        }
        case 3: {
            uint64_t late;
            CHECK(hv_vcpu_get_reg(vm->vcpu, HV_REG_X8, &late));
            push(&c->lateness, tns(late));
            if (c->lateness.n >= c->target) atomic_store(&c->ready, 2);
            break;
        }
        default: die_exit(vm, "irq hvc");
        }
    }
    CHECK(hv_vcpu_destroy(vm->vcpu));
    return NULL;
}

static bool wait_until(_Atomic uint64_t *seq, uint64_t prev, double timeout_s) {
    uint64_t deadline = ticks() + (uint64_t)(timeout_s * 1e9 * tb.denom / tb.numer);
    while (atomic_load(seq) == prev)
        if (ticks() > deadline) return false;
    return true;
}

static irq_ctx *irq_start(uint64_t spi, uint64_t period, uint64_t idle, size_t target) {
    irq_ctx *c = calloc(1, sizeof *c);
    vm_create(&c->vm, false, true);
    c->x2 = spi, c->x3 = period, c->x4 = idle, c->target = target;
    pthread_t th;
    pthread_create(&th, NULL, irq_thread, c);
    pthread_detach(th);
    uint64_t deadline = ticks() + 24000000ull * 5;
    while (atomic_load(&c->ready) == 0)
        if (ticks() > deadline) DIE("guest GIC setup timed out");
    return c;
}

static void irq_stop(irq_ctx *c) {
    uint64_t prev = atomic_load(&c->seq);
    atomic_store(&c->stop, 1);
    hv_vcpu_t v = c->vm.vcpu;
    CHECK(hv_vcpus_exit(&v, 1));
    wait_until(&c->seq, prev, 1.0);
    usleep(10000);  // let the vCPU thread destroy its vCPU before the VM goes away
    vm_destroy(&c->vm);
    free(c);
}

typedef struct { vm_t vm; _Atomic int created, done; hv_exit_reason_t reason; uint32_t ec; } wfi_ctx;

static void *wfi_thread(void *arg) {
    wfi_ctx *c = arg;
    vcpu_create(&c->vm);
    load_code(&c->vm, g_wfi, g_wfi_end);
    vcpu_reset(&c->vm, 0, 0, 0, 0, 0);
    atomic_store(&c->created, 1);
    CHECK(hv_vcpu_run(c->vm.vcpu));
    c->reason = c->vm.exit->reason;
    c->ec = EC(c->vm.exit);
    atomic_store(&c->done, 1);
    CHECK(hv_vcpu_destroy(c->vm.vcpu));
    return NULL;
}

static void t_wfi(void) {
    printf("wfi (guest executes WFI with nothing pending; watchdog kicks after 200 ms)\n");
    for (int gic = 0; gic < 2; gic++) {
        wfi_ctx c = {0};
        vm_create(&c.vm, false, gic);
        pthread_t th;
        pthread_create(&th, NULL, wfi_thread, &c);
        while (!atomic_load(&c.created)) {}
        usleep(200000);
        if (!atomic_load(&c.done)) {
            hv_vcpu_t v = c.vm.vcpu;
            hv_vcpus_exit(&v, 1);
        }
        pthread_join(th, NULL);
        printf("  %-10s first exit: reason=%u ec=0x%02x -> %s\n", gic ? "hv_gic:" : "no GIC:", c.reason, c.ec,
               c.reason == HV_EXIT_REASON_EXCEPTION && c.ec == EC_WFX ? "WFI traps to userspace"
               : c.reason == HV_EXIT_REASON_CANCELED                 ? "WFI blocks inside HVF (left only via kick)"
                                                                      : "other");
        vm_destroy(&c.vm);
    }
}

static void t_irq(void) {
    printf("irq (hv_gic_set_spi on host thread -> guest IRQ handler -> hvc exit)\n");
    uint32_t spi, count;
    CHECK(hv_gic_get_spi_interrupt_range(&spi, &count));
    for (int idle = 1; idle >= 0; idle--) {
        irq_ctx *c = irq_start(spi, 0, idle, 0);
        samples_t s = {0};
        for (int i = 0; i < 3000; i++) {
            rand_sleep_us(50, 300);
            uint64_t prev = atomic_load(&c->seq), t0 = ticks();
            CHECK(hv_gic_set_spi(spi, true));
            if (!wait_until(&c->seq, prev, 1.0)) DIE("SPI %u not delivered", spi);
            push(&s, tns(atomic_load(&c->t1) - t0));
            CHECK(hv_gic_set_spi(spi, false));
        }
        report(idle ? "set_spi -> handler, vCPU idle (WFI)" : "set_spi -> handler, vCPU busy", &s, "us", 1e3);
        printf("    userspace exits: wfx=%zu vtimer=%zu irq-reports=%zu\n", c->n_wfx, c->n_vtimer, c->n_irq);
        irq_stop(c);
    }
}

static void t_kick(void) {
    printf("kick (hv_vcpus_exit on host thread -> hv_vcpu_run returns CANCELED)\n");
    for (int idle = 0; idle < 2; idle++) {
        irq_ctx *c = irq_start(0, 0, idle, 0);
        samples_t s = {0};
        for (int i = 0; i < 3000; i++) {
            rand_sleep_us(50, 300);
            uint64_t prev = atomic_load(&c->seq), t0 = ticks();
            hv_vcpu_t v = c->vm.vcpu;
            CHECK(hv_vcpus_exit(&v, 1));
            if (!wait_until(&c->seq, prev, 1.0)) DIE("kick not observed");
            push(&s, tns(atomic_load(&c->t1) - t0));
        }
        report(idle ? "kick -> CANCELED, vCPU idle (WFI)" : "kick -> CANCELED, vCPU busy", &s, "us", 1e3);
        irq_stop(c);
    }
}

static void t_vtimer(void) {
    uint64_t frq;
    __asm__ volatile("mrs %0, cntfrq_el0" : "=r"(frq));
    printf("vtimer (guest arms CNTV for +period then WFI; lateness = handler entry - deadline)\n");
    const struct { uint64_t period_us; size_t n; } runs[] = {{100, 5000}, {1000, 2000}, {10000, 300}};
    for (int mode = 1; mode <= 2; mode++)
    for (size_t r = 0; r < sizeof runs / sizeof *runs; r++) {
        if (r == 0) printf("  idle via %s\n", mode == 1 ? "WFI (HVF in-kernel wake)" : sleep_name[g_sleep]);
        irq_ctx *c = irq_start(0, frq * runs[r].period_us / 1000000, mode, runs[r].n);
        uint64_t deadline = ticks() + 24000000ull * 20;
        while (atomic_load(&c->ready) != 2 && ticks() < deadline) usleep(1000);
        size_t got = c->lateness.n;
        char name[64];
        snprintf(name, sizeof name, "vtimer lateness, period %llu us", runs[r].period_us);
        report(name, &c->lateness, "us", 1e3);
        printf("    samples=%zu/%zu userspace exits: wfx=%zu vtimer=%zu\n", got, runs[r].n, c->n_wfx, c->n_vtimer);
        irq_stop(c);
    }
}

static void t_sleep(void) {
    printf("sleep (host thread sleeps until an absolute deadline; lateness = wake - deadline)\n");
    const uint64_t periods_us[] = {100, 1000, 10000};
    for (int api = 0; api < SLEEP_COUNT; api++)
        for (size_t r = 0; r < 3; r++) {
            samples_t s = {0};
            uint64_t per = periods_us[r] * 1000 * tb.denom / tb.numer;
            size_t n = periods_us[r] == 10000 ? 300 : 2000;
            for (size_t i = 0; i < n; i++) {
                uint64_t d = ticks() + per;
                sleep_until(api, d);
                push(&s, tns(ticks() - d));
            }
            char name[80];
            snprintf(name, sizeof name, "%s, %llu us", sleep_name[api], periods_us[r]);
            report(name, &s, "us", 1e3);
        }
}

// ---- process spawn -> first guest instruction -----------------------------------------

typedef struct { double vm_create, vcpu_create, first_run; } child_report;

static void *child_vcpu_thread(void *arg) {
    vm_t *vm = arg;
    child_report *r = (child_report *)(vm + 1);
    uint64_t t0 = ticks();
    vcpu_create(vm);
    uint64_t t1 = ticks();
    load_code(vm, g_hvc, g_hvc_end);
    vcpu_reset(vm, 0, 0, 0, 0, 0);
    run_to_hvc(vm, "child");
    r->vcpu_create = tns(t1 - t0);
    r->first_run = tns(ticks() - t1);
    return NULL;
}

static int child(const char *mode) {
    struct { vm_t vm; child_report r; } s = {0};
    if (!strcmp(mode, "vm")) {
        uint64_t t0 = ticks();
        vm_create(&s.vm, false, true);
        void *ram = mmap(NULL, 128ull << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0);
        CHECK(hv_vm_map(ram, DATA_GPA, 128ull << 20, HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC));
        s.r.vm_create = tns(ticks() - t0);
        pthread_t th;
        pthread_create(&th, NULL, child_vcpu_thread, &s.vm);
        pthread_join(th, NULL);
    }
    return write(3, &s.r, sizeof s.r) == sizeof s.r ? 0 : 1;
}

static void t_spawn(const char *self) {
    printf("spawn (posix_spawn -> child reports over a pipe)\n");
    const char *modes[] = {"noop", "vm"};
    for (int m = 0; m < 2; m++) {
        samples_t tot = {0}, vc = {0}, cc = {0}, fr = {0};
        for (int i = 0; i < 50; i++) {
            int p[2];
            if (pipe(p)) DIE("pipe");
            posix_spawn_file_actions_t fa;
            posix_spawn_file_actions_init(&fa);
            posix_spawn_file_actions_addclose(&fa, p[0]);  // before dup2: p[0] is usually fd 3
            posix_spawn_file_actions_adddup2(&fa, p[1], 3);
            char *argv[] = {(char *)self, "--child", (char *)modes[m], NULL};
            pid_t pid;
            uint64_t t0 = ticks();
            if (posix_spawn(&pid, self, &fa, NULL, argv, environ)) DIE("posix_spawn");
            close(p[1]);
            child_report r;
            if (read(p[0], &r, sizeof r) != sizeof r) DIE("child report");
            push(&tot, tns(ticks() - t0));
            close(p[0]);
            posix_spawn_file_actions_destroy(&fa);
            waitpid(pid, NULL, 0);
            if (m) push(&vc, r.vm_create), push(&cc, r.vcpu_create), push(&fr, r.first_run);
        }
        report(m ? "spawn -> VM ready -> first guest exit" : "spawn -> child main()", &tot, "ms", 1e6);
        if (m) {
            report("  in child: vm+gic+sys map+128 MiB map", &vc, "us", 1e3);
            report("  in child: hv_vcpu_create + sysregs", &cc, "us", 1e3);
            report("  in child: first hv_vcpu_run -> hvc", &fr, "us", 1e3);
        }
    }
}

// ---- main ----------------------------------------------------------------------------

int main(int argc, char **argv) {
    mach_timebase_info(&tb);
    if (argc == 3 && !strcmp(argv[1], "--child")) return child(argv[2]);
    char self[4096];
    uint32_t len = sizeof self;
    if (_NSGetExecutablePath(self, &len)) DIE("executable path");
    int first = 1;
    for (; first < argc && !strncmp(argv[first], "--", 2); first++) {
        const char *a = argv[first];
        if (!strncmp(a, "--qos=", 6)) {
            const char *q = a + 6;
            g_qos = !strcmp(q, "ui") ? QOS_CLASS_USER_INTERACTIVE : !strcmp(q, "in") ? QOS_CLASS_USER_INITIATED
                  : !strcmp(q, "ut") ? QOS_CLASS_UTILITY : !strcmp(q, "bg") ? QOS_CLASS_BACKGROUND
                  : (DIE("unknown qos %s", q), QOS_CLASS_UNSPECIFIED);
        } else if (!strcmp(a, "--rt")) {
            g_rt = true;
        } else if (!strcmp(a, "--lat0")) {
            g_lat0 = true;
        } else if (!strncmp(a, "--sleep=", 8)) {
            const char *v = a + 8;
            g_sleep = !strcmp(v, "kq") ? SLEEP_KQ : !strcmp(v, "ns") ? SLEEP_NS : !strcmp(v, "mwu") ? SLEEP_MWU
                    : (DIE("unknown sleep api %s", v), 0);
        } else {
            DIE("unknown flag %s", a);
        }
        printf("flag: %s\n", a);
    }
    apply_qos();

    static const struct { const char *name; void (*fn)(void); } tests[] = {
        {"info", t_info}, {"guestinfo", t_guestinfo}, {"lifecycle", t_lifecycle}, {"map", t_map}, {"exits", t_exits},
        {"faults", t_faults}, {"pfault", t_pfault}, {"wfi", t_wfi}, {"irq", t_irq}, {"kick", t_kick}, {"vtimer", t_vtimer}, {"sleep", t_sleep},
    };
    bool all = argc == first;
    for (size_t i = 0; i < sizeof tests / sizeof *tests; i++) {
        bool want = all;
        for (int a = first; a < argc; a++) want |= !strcmp(argv[a], tests[i].name);
        if (want) { tests[i].fn(); fflush(stdout); }
    }
    bool spawn = all;
    for (int a = first; a < argc; a++) spawn |= !strcmp(argv[a], "spawn");
    if (spawn) t_spawn(self);
    return 0;
}

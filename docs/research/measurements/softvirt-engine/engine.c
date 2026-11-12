// softvirt-engine: the floors of a translation engine's core mechanisms, modelled in C,
// in one process, no host trap anywhere: what a purpose-built engine could cost where
// QEMU's TCG (PM M521) pays most. A guest's memory is one host mapping (its "physical"
// memory); its address spaces are 4-level page tables kept in that memory (9 bits a
// level, 4 KiB pages); a software TLB tagged by address space (an ASID) translates.
// Each test times a batch of operations a sample and prints n, p50, p90, p99 and max,
// per operation in nanoseconds (per 16 faults for the fault tests, as M519 and M521
// count them). These are C models of what translated code would do, compiled; they
// bound a design, they are not a translator. Method and results:
// docs/research/platform-measurements.md (the softvirt entries).
//
//   cc -O2 -o engine engine.c && ./engine [-n N]

#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#if defined(__APPLE__)
#include <mach/mach_time.h>
#endif

#define PAGE 4096ULL
#define TLB_BITS 12
#define TLB_SIZE (1u << TLB_BITS)
#define GMEM (1ULL << 30) // 1 GiB of guest physical memory
#define BATCH 256

static size_t n = 20000;

static uint64_t now_ns(void) {
#if defined(__APPLE__)
    static mach_timebase_info_data_t tb;
    if (tb.denom == 0)
        mach_timebase_info(&tb);
    return mach_absolute_time() * tb.numer / tb.denom;
#else
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
#endif
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static uint64_t pct(const uint64_t *s, size_t len, unsigned p) {
    size_t rank = (p * len + 99) / 100;
    return s[rank ? rank - 1 : 0];
}

// Samples are per batch; reported per operation (`per` operations a sample).
static void report(const char *test, const char *what, uint64_t *s, size_t len, unsigned per) {
    qsort(s, len, sizeof *s, cmp_u64);
    printf("| %s | %s | %zu | %.1f | %.1f | %.1f | %.1f |\n", test, what, len,
           (double)pct(s, len, 50) / per, (double)pct(s, len, 90) / per,
           (double)pct(s, len, 99) / per, (double)s[len - 1] / per);
    fflush(stdout);
}

// --- the guest machine ---------------------------------------------------------------

typedef struct {
    uint64_t vpage;   // the guest virtual page this entry holds, or ~0
    uint64_t asid;    // the address space it belongs to
    uintptr_t addend; // host address = guest virtual address + addend
    uint64_t pad;
} tlb_entry;

typedef struct {
    tlb_entry tlb[TLB_SIZE];
    uint8_t *mem;      // guest physical memory
    uint64_t asid;     // the current address space
    uint64_t root;     // its top-level table's guest physical address
    uint64_t next;     // the next free guest physical page
    uint64_t regs[31]; // the guest's general registers
    uint64_t saved[31];
    int kernel; // the guest's privilege: 0 user, 1 kernel
    uint64_t faults;
} cpu_t;

#define PRESENT 1ULL
#define FRAME (~(PAGE - 1) & ((1ULL << 52) - 1))

static uint64_t alloc_page(cpu_t *c) {
    if (c->next + PAGE > GMEM) {
        fprintf(stderr, "guest memory exhausted\n");
        exit(3);
    }
    uint64_t p = c->next;
    c->next += PAGE;
    memset(c->mem + p, 0, PAGE);
    return p;
}

static uint64_t *table(cpu_t *c, uint64_t gpa) { return (uint64_t *)(void *)(c->mem + gpa); }

// The guest kernel's part of mapping a page: what its page fault handler does.
static void map_page(cpu_t *c, uint64_t root, uint64_t va, uint64_t frame) {
    uint64_t t = root;
    for (int level = 3; level > 0; level--) {
        uint64_t *e = &table(c, t)[(va >> (12 + 9 * level)) & 511];
        if (!(*e & PRESENT))
            *e = alloc_page(c) | PRESENT;
        t = *e & FRAME;
    }
    table(c, t)[(va >> 12) & 511] = frame | PRESENT;
}

// The walk a TLB miss makes: 0 where the page is not present.
static uint64_t walk(cpu_t *c, uint64_t va) {
    uint64_t t = c->root;
    for (int level = 3; level >= 0; level--) {
        uint64_t e = table(c, t)[(va >> (12 + 9 * level)) & 511];
        if (!(e & PRESENT))
            return 0;
        t = e & FRAME;
    }
    return t | PRESENT;
}

// A guest page fault, delivered inside the engine: the guest enters its kernel (its
// registers saved, its privilege raised), whose handler gives the page a zeroed frame,
// and returns to retry the access.
static __attribute__((noinline)) void guest_fault(cpu_t *c, uint64_t va) {
    memcpy(c->saved, c->regs, sizeof c->regs);
    c->kernel = 1;
    map_page(c, c->root, va & ~(PAGE - 1), alloc_page(c));
    c->faults++;
    c->kernel = 0;
    memcpy(c->regs, c->saved, sizeof c->regs);
}

// The entry a page takes: its address space mixed into the index, so that two address
// spaces using the same addresses do not evict each other.
static inline unsigned slot(uint64_t va, uint64_t asid) {
    return (unsigned)(((va >> 12) ^ (asid * 0x9E3779B1u)) & (TLB_SIZE - 1));
}

static __attribute__((noinline)) uint8_t *tlb_miss(cpu_t *c, uint64_t va) {
    uint64_t pte = walk(c, va);
    if (!pte) {
        guest_fault(c, va);
        pte = walk(c, va);
    }
    tlb_entry *e = &c->tlb[slot(va, c->asid)];
    e->vpage = va >> 12;
    e->asid = c->asid;
    e->addend = (uintptr_t)c->mem + (pte & FRAME) - (va & ~(PAGE - 1));
    return (uint8_t *)(uintptr_t)(va + e->addend);
}

static inline uint8_t *translate(cpu_t *c, uint64_t va) {
    tlb_entry *e = &c->tlb[slot(va, c->asid)];
    if (__builtin_expect(e->vpage == va >> 12 && e->asid == c->asid, 1))
        return (uint8_t *)(uintptr_t)(va + e->addend);
    return tlb_miss(c, va);
}

static void tlb_flush(cpu_t *c) {
    for (unsigned i = 0; i < TLB_SIZE; i++)
        c->tlb[i].vpage = ~0ULL;
}

static cpu_t *machine(void) {
    cpu_t *c = calloc(1, sizeof *c);
    c->mem = mmap(NULL, GMEM, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (!c || c->mem == MAP_FAILED) {
        perror("guest memory");
        exit(2);
    }
    // Every frame touched once now, so that no host fault lands in a measurement.
    for (uint64_t off = 0; off < GMEM; off += PAGE)
        c->mem[off] = 0;
    c->next = PAGE; // frame 0 unused, so a zero entry means "absent"
    tlb_flush(c);
    return c;
}

static void reset(cpu_t *c) {
    c->next = PAGE;
    tlb_flush(c);
    c->asid = 0;
}

static uint64_t new_space(cpu_t *c, uint64_t base, size_t pages) {
    uint64_t root = alloc_page(c);
    for (size_t i = 0; i < pages; i++)
        map_page(c, root, base + i * PAGE, alloc_page(c));
    return root;
}

static volatile uint64_t sink;

// --- loads: native, a TLB hit, a TLB miss and walk ----------------------------------

static void t_loads(void) {
    uint64_t *s = calloc(n, sizeof *s);
    size_t pages = 64;
    uint8_t *host = mmap(NULL, pages * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    memset(host, 1, pages * PAGE);
    for (size_t i = 0; i < n; i++) {
        uint64_t acc = 0, t = now_ns();
        for (unsigned k = 0; k < BATCH; k++)
            acc += *(volatile uint64_t *)(void *)(host + (k % pages) * PAGE + (k & 7) * 8);
        s[i] = now_ns() - t;
        sink += acc;
    }
    report("load", "native, 64 pages", s, n, BATCH);
    size_t many_host = 65536;
    uint8_t *big = mmap(NULL, many_host * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    memset(big, 1, many_host * PAGE);
    uint64_t y = 88172645463325252ULL;
    for (size_t i = 0; i < n; i++) {
        uint64_t acc = 0, t = now_ns();
        for (unsigned k = 0; k < BATCH; k++) {
            y ^= y << 13, y ^= y >> 7, y ^= y << 17;
            acc += *(volatile uint64_t *)(void *)(big + (y % many_host) * PAGE);
        }
        s[i] = now_ns() - t;
        sink += acc;
    }
    report("load", "native, 256 MiB, random", s, n, BATCH);
    munmap(big, many_host * PAGE);

    cpu_t *c = machine();
    uint64_t base = 0x400000;
    c->root = new_space(c, base, pages);
    for (size_t i = 0; i < n; i++) {
        uint64_t acc = 0, t = now_ns();
        for (unsigned k = 0; k < BATCH; k++)
            acc += *(volatile uint64_t *)(void *)translate(c, base + (k % pages) * PAGE + (k & 7) * 8);
        s[i] = now_ns() - t;
        sink += acc;
    }
    report("load", "software MMU, TLB hit (64 pages)", s, n, BATCH);

    // Misses: 65,536 pages (256 MiB) visited in a pseudo-random order through a TLB of
    // 4,096 entries, each a 4-level walk.
    reset(c);
    size_t many = 65536;
    c->root = new_space(c, base, many);
    uint64_t x = 88172645463325252ULL;
    for (size_t i = 0; i < n; i++) {
        uint64_t acc = 0, t = now_ns();
        for (unsigned k = 0; k < BATCH; k++) {
            x ^= x << 13, x ^= x >> 7, x ^= x << 17;
            acc += *(volatile uint64_t *)(void *)translate(c, base + (x % many) * PAGE);
        }
        s[i] = now_ns() - t;
        sink += acc;
    }
    report("load", "software MMU, miss and 4-level walk (256 MiB, random)", s, n, BATCH);
    munmap(c->mem, GMEM);
    free(c);
}

// --- address-space switches: tagged TLB against a flush ------------------------------

static void t_switch(void) {
    uint64_t *s = calloc(n, sizeof *s);
    cpu_t *c = machine();
    uint64_t base = 0x400000, roots[2];
    size_t pages = 64;
    roots[0] = new_space(c, base, pages);
    roots[1] = new_space(c, base, pages);
    // Every load after a switch touches 16 pages, as a process resumed would.
    for (int flush = 0; flush < 2; flush++) {
        for (size_t i = 0; i < n; i++) {
            uint64_t acc = 0, t = now_ns();
            for (unsigned k = 0; k < BATCH / 16; k++) {
                c->asid = k & 1;
                c->root = roots[k & 1];
                if (flush)
                    tlb_flush(c);
                for (unsigned p = 0; p < 16; p++)
                    acc += *(volatile uint64_t *)(void *)translate(c, base + p * PAGE);
            }
            s[i] = now_ns() - t;
            sink += acc;
        }
        report("switch", flush ? "address-space switch, TLB flushed, then 16 pages touched"
                               : "address-space switch, TLB tagged, then 16 pages touched",
               s, n, BATCH / 16);
    }
    munmap(c->mem, GMEM);
    free(c);
}

// --- a translated system call: the engine's part ---------------------------------------

typedef uint64_t (*handler_t)(cpu_t *);
static uint64_t sys_getppid(cpu_t *c) { return c->regs[0] + 1; }
static handler_t table_[512];

// The guest's `syscall` (or `svc`) as translated code would run it: its registers saved
// to its kernel's stack, its privilege raised, the handler its number names called, the
// result put back, its registers and privilege restored. The guest kernel's own entry
// and exit code is not modelled: translated, it costs what it does natively, times the
// translation's factor (M521: 1.1 to 1.3).
static __attribute__((noinline)) void guest_syscall(cpu_t *c) {
    memcpy(c->saved, c->regs, sizeof c->regs);
    c->kernel = 1;
    uint64_t nr = c->regs[8] & 511;
    uint64_t r = table_[nr](c);
    c->kernel = 0;
    memcpy(c->regs, c->saved, sizeof c->regs);
    c->regs[0] = r;
}

static void t_syscall(void) {
    uint64_t *s = calloc(n, sizeof *s);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        for (unsigned k = 0; k < BATCH; k++)
            getppid();
        s[i] = now_ns() - t;
    }
    report("syscall", "native getppid", s, n, BATCH);
    cpu_t *c = machine();
    for (int i = 0; i < 512; i++)
        table_[i] = sys_getppid;
    c->regs[8] = 110;
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        for (unsigned k = 0; k < BATCH; k++)
            guest_syscall(c);
        s[i] = now_ns() - t;
        sink += c->regs[0];
    }
    report("syscall", "translated guest system call, the engine's part", s, n, BATCH);
    munmap(c->mem, GMEM);
    free(c);
}

// --- first-touch faults: native, and delivered inside the engine ---------------------

static void t_fault(void) {
    size_t count = n / 4 ? n / 4 : 1;
    uint64_t *s = calloc(count, sizeof *s);
    for (size_t i = 0; i < count; i++) {
        uint8_t *p = mmap(NULL, 16 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
        uint64_t t = now_ns();
        for (unsigned k = 0; k < 16; k++)
            p[k * PAGE] = 1;
        s[i] = now_ns() - t;
        munmap(p, 16 * PAGE);
    }
    report("fault", "native, 16 first touches", s, count, 1);

    cpu_t *c = machine();
    uint64_t base = 0x400000;
    for (size_t i = 0; i < count; i++) {
        if (c->next > GMEM - (64ULL << 20)) { // keep the guest's memory in bounds
            reset(c);
        }
        uint64_t root = alloc_page(c);
        c->root = root;
        c->asid = i & 0xffff;
        uint64_t t = now_ns();
        for (unsigned k = 0; k < 16; k++)
            *translate(c, base + k * PAGE) = 1;
        s[i] = now_ns() - t;
    }
    report("fault", "in-engine, 16 first touches (zeroed frame, page tables built)", s, count, 1);
    munmap(c->mem, GMEM);
    free(c);
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "-n"))
        n = strtoul(argv[2], NULL, 10);
    if (n < 4)
        return 2;
    printf("| test | operation | n | p50 ns | p90 ns | p99 ns | max ns |\n|---|---|---|---|---|---|---|\n");
    t_loads();
    t_switch();
    t_syscall();
    t_fault();
    return 0;
}

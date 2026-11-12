// softvirt-traps (macOS, arm64): what the primitives cost that a rootless software
// hypervisor would serve a guest with on a Mac that gives it no Hypervisor.framework
// (a macOS VM without nested virtualization). macOS has no seccomp: a guest's `svc`
// would enter XNU, so its code reaches the hypervisor only as `brk` (a rewritten
// `svc`) or a fault, raised as a signal in its own process or as a Mach exception to
// another. Each test times one round trip per iteration and prints n, p50, p90, p99
// and max in nanoseconds (mach_absolute_time: one tick is 41.67 ns on Apple silicon).
// `taskport` probes whether a parent can drive its child's memory and threads without
// root or entitlements. Method and results: docs/research/platform-measurements.md.
//
//   cc -O2 -Wall -Wextra -o traps macos.c && ./traps            # every test
//   ./traps -n 50000 sigtrap child-brk                          # selected tests

#include <errno.h>
#include <libkern/OSCacheControl.h>
#include <mach-o/dyld.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/mach_vm.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysctl.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#if !defined(__aarch64__)
#error "arm64 only: Intel Macs are unsupported"
#endif

extern char **environ;

static size_t n = 20000;
static const size_t warm = 1000;
static mach_timebase_info_data_t tb;
static char self_path[4096];

static uint64_t now_ns(void) { return mach_absolute_time() * tb.numer / tb.denom; }

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static uint64_t pct(const uint64_t *s, size_t len, unsigned p) {
    size_t rank = (p * len + 99) / 100;
    return s[rank ? rank - 1 : 0];
}

static void report(const char *test, const char *variant, uint64_t *s, size_t len) {
    if (len == 0) {
        printf("| %s | %s | 0 | | | | |\n", test, variant);
        return;
    }
    qsort(s, len, sizeof *s, cmp_u64);
    printf("| %s | %s | %zu | %llu | %llu | %llu | %llu |\n", test, variant, len,
           (unsigned long long)pct(s, len, 50), (unsigned long long)pct(s, len, 90),
           (unsigned long long)pct(s, len, 99), (unsigned long long)s[len - 1]);
    fflush(stdout);
}

static void note(const char *test, const char *variant, const char *what) {
    printf("| %s | %s | %s | | | | |\n", test, variant, what);
    fflush(stdout);
}

static int wait_child(pid_t pid) {
    int status;
    while (waitpid(pid, &status, 0) < 0)
        if (errno != EINTR)
            return -1;
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

// arm64's thread state as the kernel gives a plain (not arm64e) process.
typedef struct {
    uint64_t x[29], fp, lr, sp, pc;
    uint32_t cpsr, pad;
} state64;

// mach_exception_raise_state (mach_exc.defs, id 2406) and its reply, as MIG lays them
// out with both of an exception's codes (MACH_EXCEPTION_CODES gives two).
#pragma pack(push, 4)
typedef struct {
    mach_msg_header_t head;
    NDR_record_t ndr;
    exception_type_t exception;
    mach_msg_type_number_t code_count;
    int64_t code[2];
    int flavor;
    mach_msg_type_number_t state_count;
    natural_t state[ARM_THREAD_STATE64_COUNT];
    mach_msg_trailer_t trailer;
} exc_request;

typedef struct {
    mach_msg_header_t head;
    NDR_record_t ndr;
    kern_return_t ret;
    int flavor;
    mach_msg_type_number_t state_count;
    natural_t state[ARM_THREAD_STATE64_COUNT];
} exc_reply;
#pragma pack(pop)

static mach_port_t new_port(void) {
    mach_port_t port = MACH_PORT_NULL;
    mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &port);
    mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND);
    return port;
}

// Answers one exception and waits for the next, in one mach_msg, which sends from and
// receives into the same buffer: the faulting thread resumes at the instruction after
// the one that raised it, x0 = 0. A message of another shape is answered with failure
// (the kernel then goes on as if no one had caught it) rather than left to hang its
// thread. Returns the receive's result (MACH_RCV_TIMED_OUT once `timeout_ms` passes).
static kern_return_t serve_exceptions(mach_port_t port, size_t limit, unsigned timeout_ms,
                                      size_t *served) {
    union {
        exc_request req;
        exc_reply rep;
    } m;
    kern_return_t kr = mach_msg(&m.req.head, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, sizeof m, port,
                                timeout_ms, MACH_PORT_NULL);
    while (kr == KERN_SUCCESS) {
        int ours = m.req.head.msgh_id == 2406 && m.req.code_count == 2 &&
                   m.req.state_count == ARM_THREAD_STATE64_COUNT;
        if (!ours)
            fprintf(stderr, "unexpected exception message: id %d, %u codes, %u state words\n",
                    m.req.head.msgh_id, m.req.code_count, m.req.state_count);
        state64 st;
        memcpy(&st, m.req.state, sizeof st);
        st.pc += 4;
        st.x[0] = 0;
        mach_msg_bits_t bits = m.req.head.msgh_bits;
        mach_port_t reply = m.req.head.msgh_remote_port;
        mach_msg_id_t id = m.req.head.msgh_id;
        memset(&m.rep, 0, sizeof m.rep);
        m.rep.head.msgh_bits = MACH_MSGH_BITS(MACH_MSGH_BITS_REMOTE(bits), 0);
        m.rep.head.msgh_remote_port = reply;
        m.rep.head.msgh_size = sizeof m.rep;
        m.rep.head.msgh_id = id + 100;
        m.rep.ndr = NDR_record;
        m.rep.ret = ours ? KERN_SUCCESS : KERN_FAILURE;
        m.rep.flavor = ARM_THREAD_STATE64;
        m.rep.state_count = ARM_THREAD_STATE64_COUNT;
        memcpy(m.rep.state, &st, sizeof st);
        if (!ours) {
            mach_msg(&m.rep.head, MACH_SEND_MSG, sizeof m.rep, 0, MACH_PORT_NULL,
                     MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL);
            return KERN_FAILURE;
        }
        (*served)++;
        if (*served >= limit) {
            return mach_msg(&m.rep.head, MACH_SEND_MSG, sizeof m.rep, 0, MACH_PORT_NULL,
                            MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL);
        }
        kr = mach_msg(&m.rep.head, MACH_SEND_MSG | MACH_RCV_MSG | MACH_RCV_TIMEOUT, sizeof m.rep,
                      sizeof m, port, timeout_ms, MACH_PORT_NULL);
    }
    return kr;
}

static void brk_loop(uint64_t *s, size_t total) {
    for (size_t i = 0; i < total; i++) {
        uint64_t t = now_ns();
        __asm__ volatile("brk #0x55" ::: "x0", "memory");
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
}

// --- native ---------------------------------------------------------------------------

static void t_native(void) {
    uint64_t *s = calloc(n, sizeof *s);
    for (size_t i = 0; i < warm; i++)
        getppid();
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        for (int k = 0; k < 64; k++)
            getppid();
        s[i] = (now_ns() - t) / 64;
    }
    report("native", "getppid (each of 64 in a row)", s, n);
}

// --- sigtrap: brk to a signal handler in the guest's own process ----------------------

static void on_sigtrap(int sig, siginfo_t *si, void *ctx) {
    (void)sig;
    (void)si;
    ucontext_t *uc = ctx;
    uc->uc_mcontext->__ss.__pc += 4;
    uc->uc_mcontext->__ss.__x[0] = 0;
}

static void t_sigtrap(void) {
    uint64_t *s = calloc(n, sizeof *s);
    struct sigaction sa = {0};
    sa.sa_sigaction = on_sigtrap;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGTRAP, &sa, NULL);
    brk_loop(s, warm + n);
    report("sigtrap", "brk, in-process signal handler", s, n);
}

// --- machexc: brk as a Mach exception to a thread of the same process -----------------

static mach_port_t g_port;

static void *exception_thread(void *arg) {
    size_t served = 0;
    serve_exceptions(g_port, (size_t)(uintptr_t)arg, 60000, &served);
    return NULL;
}

static void t_machexc(void) {
    uint64_t *s = calloc(n, sizeof *s);
    g_port = new_port();
    kern_return_t kr = thread_set_exception_ports(
        mach_thread_self(), EXC_MASK_BREAKPOINT, g_port,
        (exception_behavior_t)(EXCEPTION_STATE | MACH_EXCEPTION_CODES), ARM_THREAD_STATE64);
    if (kr != KERN_SUCCESS) {
        note("machexc", "brk, thread of the same process", mach_error_string(kr));
        return;
    }
    pthread_t th;
    pthread_create(&th, NULL, exception_thread, (void *)(uintptr_t)(warm + n));
    brk_loop(s, warm + n);
    pthread_join(th, NULL);
    report("machexc", "brk, Mach exception to a thread of the same process", s, n);
}

// --- child-brk / child-fault: exceptions of a spawned child, served by its parent -----

static pid_t spawn_child(const char *mode, mach_port_t exc_port, exception_mask_t mask,
                         mach_port_t bootstrap) {
    posix_spawnattr_t attr;
    posix_spawnattr_init(&attr);
    if (exc_port != MACH_PORT_NULL)
        posix_spawnattr_setexceptionports_np(
            &attr, mask, exc_port, (exception_behavior_t)(EXCEPTION_STATE | MACH_EXCEPTION_CODES),
            ARM_THREAD_STATE64);
    if (bootstrap != MACH_PORT_NULL)
        posix_spawnattr_setspecialport_np(&attr, bootstrap, TASK_BOOTSTRAP_PORT);
    char count[32];
    snprintf(count, sizeof count, "%zu", n);
    char *argv[] = {self_path, "--child", (char *)mode, count, NULL};
    pid_t pid = -1;
    fflush(stdout);
    int err = posix_spawn(&pid, self_path, NULL, &attr, argv, environ);
    posix_spawnattr_destroy(&attr);
    return err ? -1 : pid;
}

static void serve_child(const char *test, const char *mode, exception_mask_t mask) {
    mach_port_t port = new_port();
    pid_t pid = spawn_child(mode, port, mask, MACH_PORT_NULL);
    if (pid < 0) {
        note(test, mode, "spawn failed");
        return;
    }
    size_t served = 0;
    kern_return_t kr = serve_exceptions(port, warm + n, 5000, &served);
    if (wait_child(pid))
        note(test, mode, kr == KERN_SUCCESS ? "child failed" : mach_error_string(kr));
    mach_port_deallocate(mach_task_self(), port);
    mach_port_mod_refs(mach_task_self(), port, MACH_PORT_RIGHT_RECEIVE, -1);
}

static void t_child_brk(void) {
    serve_child("child-brk", "brk", EXC_MASK_BREAKPOINT);
}

static void t_child_fault(void) {
    serve_child("child-fault", "fault", EXC_MASK_BAD_ACCESS);
}

static int child_main(const char *mode) {
    uint64_t *s = calloc(n, sizeof *s);
    if (!strcmp(mode, "brk")) {
        brk_loop(s, warm + n);
        report("child-brk", "brk, Mach exception to the parent", s, n);
        return 0;
    }
    if (!strcmp(mode, "fault")) {
        size_t page = (size_t)getpagesize();
        char *p = mmap(NULL, page, PROT_NONE, MAP_PRIVATE | MAP_ANON, -1, 0);
        for (size_t i = 0; i < warm + n; i++) {
            uint64_t v, t = now_ns();
            __asm__ volatile("ldr %0, [%1]" : "=r"(v) : "r"(p) : "memory");
            uint64_t d = now_ns() - t;
            (void)v;
            if (i >= warm)
                s[i - warm] = d;
        }
        report("child-fault", "load of a PROT_NONE page, Mach exception to the parent", s, n);
        return 0;
    }
    if (!strcmp(mode, "taskport")) {
        // Send our own task port to the parent through the bootstrap port it gave us.
        mach_port_t bp = MACH_PORT_NULL;
        task_get_special_port(mach_task_self(), TASK_BOOTSTRAP_PORT, &bp);
        struct {
            mach_msg_header_t head;
            mach_msg_body_t body;
            mach_msg_port_descriptor_t task;
        } msg;
        memset(&msg, 0, sizeof msg);
        msg.head.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0) | MACH_MSGH_BITS_COMPLEX;
        msg.head.msgh_remote_port = bp;
        msg.head.msgh_size = sizeof msg;
        msg.body.msgh_descriptor_count = 1;
        msg.task.name = mach_task_self();
        msg.task.disposition = MACH_MSG_TYPE_COPY_SEND;
        msg.task.type = MACH_MSG_PORT_DESCRIPTOR;
        kern_return_t kr = mach_msg(&msg.head, MACH_SEND_MSG | MACH_SEND_TIMEOUT, sizeof msg, 0,
                                    MACH_PORT_NULL, 2000, MACH_PORT_NULL);
        printf("- child: sending its own task port: %s\n", mach_error_string(kr));
        fflush(stdout);
        sleep(30);
        return 0;
    }
    return 2;
}

// --- taskport: can a parent drive its child, as a hypervisor would its guest's? -------

static void t_taskport(void) {
    mach_port_t port = new_port();
    pid_t pid = spawn_child("taskport", MACH_PORT_NULL, 0, port);
    if (pid < 0) {
        note("taskport", "spawn", "failed");
        return;
    }
    mach_port_t task = MACH_PORT_NULL;
    kern_return_t kr = task_for_pid(mach_task_self(), pid, &task);
    printf("- parent: task_for_pid(child): %s\n", mach_error_string(kr));
    struct {
        mach_msg_header_t head;
        mach_msg_body_t body;
        mach_msg_port_descriptor_t task;
        mach_msg_trailer_t trailer;
    } msg;
    memset(&msg, 0, sizeof msg);
    kr = mach_msg(&msg.head, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, sizeof msg, port, 3000,
                  MACH_PORT_NULL);
    printf("- parent: receiving the child's task port: %s\n", mach_error_string(kr));
    if (kr == KERN_SUCCESS && (msg.head.msgh_bits & MACH_MSGH_BITS_COMPLEX))
        task = msg.task.name;
    if (task == MACH_PORT_NULL) {
        note("taskport", "parent drives child", "no task port");
        kill(pid, SIGKILL);
        wait_child(pid);
        return;
    }
    mach_vm_address_t addr = 0;
    kr = mach_vm_allocate(task, &addr, 16384, VM_FLAGS_ANYWHERE);
    printf("- parent: mach_vm_allocate in the child: %s\n", mach_error_string(kr));
    thread_act_array_t threads;
    mach_msg_type_number_t count = 0;
    kern_return_t tk = task_threads(task, &threads, &count);
    printf("- parent: task_threads(child): %s (%u threads)\n", mach_error_string(tk), count);
    fflush(stdout);
    if (kr == KERN_SUCCESS) {
        // Guest memory shared into the child, one 16 KiB page at a time.
        size_t pages = 512;
        mach_vm_address_t src = 0;
        mach_vm_allocate(mach_task_self(), &src, pages * 16384, VM_FLAGS_ANYWHERE);
        memory_object_size_t size = pages * 16384;
        mach_port_t entry = MACH_PORT_NULL;
        mach_make_memory_entry_64(mach_task_self(), &size, src, VM_PROT_READ | VM_PROT_WRITE,
                                  &entry, MACH_PORT_NULL);
        uint64_t *map = calloc(n, sizeof *map), *unmap = calloc(n, sizeof *unmap);
        size_t done = 0;
        for (size_t i = 0; i < n; i++) {
            mach_vm_address_t at = 0;
            uint64_t t = now_ns();
            kern_return_t m = mach_vm_map(task, &at, 16384, 0, VM_FLAGS_ANYWHERE, entry,
                                          (i % pages) * 16384, FALSE, VM_PROT_READ | VM_PROT_WRITE,
                                          VM_PROT_READ | VM_PROT_WRITE, VM_INHERIT_NONE);
            map[i] = now_ns() - t;
            if (m != KERN_SUCCESS) {
                note("taskport", "mach_vm_map into the child", mach_error_string(m));
                break;
            }
            t = now_ns();
            mach_vm_deallocate(task, at, 16384);
            unmap[i] = now_ns() - t;
            done++;
        }
        report("taskport", "mach_vm_map a 16 KiB page into the child", map, done);
        report("taskport", "mach_vm_deallocate it", unmap, done);
    }
    if (tk == KERN_SUCCESS && count > 0) {
        uint64_t *s = calloc(n, sizeof *s);
        for (size_t i = 0; i < n; i++) {
            state64 st;
            mach_msg_type_number_t c = ARM_THREAD_STATE64_COUNT;
            uint64_t t = now_ns();
            thread_get_state(threads[0], ARM_THREAD_STATE64, (thread_state_t)&st, &c);
            s[i] = now_ns() - t;
        }
        report("taskport", "thread_get_state of the child's thread", s, n);
    }
    kill(pid, SIGKILL);
    wait_child(pid);
}

// --- fault: a guest page fault served in the guest's own process ----------------------

static char *g_page;
static size_t g_pagesize;

static void on_fault(int sig, siginfo_t *si, void *ctx) {
    (void)sig;
    (void)ctx;
    char *page = (char *)((uintptr_t)si->si_addr & ~(uintptr_t)(g_pagesize - 1));
    if (page != g_page)
        abort();
    mprotect(page, g_pagesize, PROT_READ | PROT_WRITE);
}

static void t_fault(void) {
    uint64_t *s = calloc(n, sizeof *s);
    g_pagesize = (size_t)getpagesize();
    struct sigaction sa = {0};
    sa.sa_sigaction = on_fault;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    g_page = mmap(NULL, g_pagesize, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    g_page[0] = 1;
    for (size_t i = 0; i < warm + n; i++) {
        mprotect(g_page, g_pagesize, PROT_NONE);
        uint64_t t = now_ns();
        ((volatile char *)g_page)[0] = 2;
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
    report("fault", "signal, handler mprotects the page", s, n);
}

// --- jit: what a translator pays to publish code on Apple silicon ---------------------

static void t_jit(void) {
    size_t len = 16384;
    uint32_t *code = mmap(NULL, len, PROT_READ | PROT_WRITE | PROT_EXEC,
                          MAP_PRIVATE | MAP_ANON | MAP_JIT, -1, 0);
    if (code == MAP_FAILED) {
        note("jit", "MAP_JIT", strerror(errno));
        return;
    }
    uint64_t *s = calloc(n, sizeof *s);
    for (size_t i = 0; i < warm + n; i++) {
        uint64_t t = now_ns();
        pthread_jit_write_protect_np(0);
        pthread_jit_write_protect_np(1);
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
    report("jit", "write-protect off and on again", s, n);
    for (size_t i = 0; i < warm + n; i++) {
        uint32_t imm = (uint32_t)(i & 0xffff);
        uint64_t t = now_ns();
        pthread_jit_write_protect_np(0);
        code[0] = 0x52800000u | (imm << 5); // mov w0, #imm
        code[1] = 0xd65f03c0u;              // ret
        pthread_jit_write_protect_np(1);
        sys_icache_invalidate(code, 8);
        uint32_t got = ((uint32_t(*)(void))(uintptr_t)code)();
        uint64_t d = now_ns() - t;
        if (got != imm)
            abort();
        if (i >= warm)
            s[i - warm] = d;
    }
    report("jit", "write 2 instructions, invalidate, call", s, n);
}

// --- the host ------------------------------------------------------------------------

static void print_sysctl(const char *name) {
    char buf[256] = {0};
    size_t len = sizeof buf - 1;
    if (sysctlbyname(name, buf, &len, NULL, 0) == 0) {
        if (len == sizeof(int64_t) || len == sizeof(int32_t)) {
            int64_t v = len == sizeof(int64_t) ? *(int64_t *)buf : *(int32_t *)buf;
            printf("- %s: %lld\n", name, (long long)v);
        } else {
            printf("- %s: %s\n", name, buf);
        }
    } else {
        printf("- %s: (none)\n", name);
    }
}

static void print_host(void) {
    struct utsname u;
    uname(&u);
    printf("- kernel: %s %s %s\n", u.sysname, u.release, u.machine);
    print_sysctl("kern.osproductversion");
    print_sysctl("machdep.cpu.brand_string");
    print_sysctl("hw.perflevel0.physicalcpu");
    print_sysctl("hw.perflevel1.physicalcpu");
    print_sysctl("kern.hv_support");
    print_sysctl("kern.hv_vmm_present");
    printf("- timebase: %u/%u ns a tick; page %d bytes\n", tb.numer, tb.denom, getpagesize());
}

struct test {
    const char *name;
    void (*run)(void);
};

static const struct test tests[] = {
    {"native", t_native},       {"sigtrap", t_sigtrap},         {"machexc", t_machexc},
    {"child-brk", t_child_brk}, {"child-fault", t_child_fault}, {"taskport", t_taskport},
    {"fault", t_fault},         {"jit", t_jit},
};

int main(int argc, char **argv) {
    mach_timebase_info(&tb);
    uint32_t size = sizeof self_path;
    if (_NSGetExecutablePath(self_path, &size))
        return 2;
    if (argc == 4 && !strcmp(argv[1], "--child")) {
        n = strtoul(argv[3], NULL, 10);
        return child_main(argv[2]);
    }
    int first = 1;
    if (argc > 2 && !strcmp(argv[1], "-n")) {
        n = strtoul(argv[2], NULL, 10);
        first = 3;
    }
    if (n == 0)
        return 2;
    print_host();
    printf("\n| test | variant | n | p50 ns | p90 ns | p99 ns | max ns |\n");
    printf("|---|---|---|---|---|---|---|\n");
    fflush(stdout);
    for (size_t t = 0; t < sizeof tests / sizeof tests[0]; t++) {
        int chosen = first >= argc;
        for (int a = first; a < argc; a++)
            chosen |= !strcmp(argv[a], tests[t].name);
        if (!chosen)
            continue;
        pid_t pid = fork();
        if (pid == 0) {
            tests[t].run();
            fflush(stdout);
            _exit(0);
        }
        if (wait_child(pid))
            note(tests[t].name, "test", "failed");
    }
    return 0;
}

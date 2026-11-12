// softvirt-traps (Linux): what the primitives cost that a rootless software hypervisor
// would serve a guest with on a host that gives it no hardware virtualization. The
// guest's code runs natively in a child process; its system calls and faults come back
// to the hypervisor. Each test times one round trip per iteration, as the guest sees
// it, and prints n, p50, p90, p99 and max in nanoseconds. `maps` measures how host
// mappings of guest pages scale, up to the host's map-count limit.
// Method and results: docs/research/platform-measurements.md (the softvirt entries).
//
//   cc -O2 -Wall -Wextra -o traps linux.c && ./traps          # every test
//   ./traps -n 50000 sigsys handoff notify                    # selected tests
//
// Needs no root: seccomp filters need only no_new_privs, ptrace only one's own child.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/futex.h>
#include <linux/seccomp.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#ifndef NT_PRSTATUS
#define NT_PRSTATUS 1
#endif
#ifndef PTRACE_SYSEMU
#define PTRACE_SYSEMU 31
#endif
#ifndef SECCOMP_IOCTL_NOTIF_SET_FLAGS
#define SECCOMP_IOCTL_NOTIF_SET_FLAGS SECCOMP_IOW(4, __u64)
#endif
#ifndef SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP
#define SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP 1UL
#endif

// A system call number no architecture assigns: the guest's "system call", which the
// filters send to the hypervisor.
#define GUEST_NR 1023
#define PAGE 4096UL
#define HUGE (2UL << 20)
#define SPIN (1L << 22)

static size_t n = 20000;
static const size_t warm = 1000;
static int cpu_a = -1, cpu_b = -1;

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

// Nearest-rank percentile of sorted samples.
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

static void *shared(size_t len) {
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap shared");
        exit(1);
    }
    return p;
}

static void pin(int cpu) {
    if (cpu < 0)
        return;
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    if (sched_setaffinity(0, sizeof set, &set))
        perror("sched_setaffinity");
}

static void relax(void) {
#if defined(__x86_64__)
    __builtin_ia32_pause();
#elif defined(__aarch64__)
    __asm__ volatile("yield");
#endif
}

static long futex(uint32_t *word, int op, uint32_t val) {
    return syscall(SYS_futex, word, op, val, NULL, NULL, 0);
}

// One direction of a channel between two processes: a counter, and whether its reader
// sleeps on it (so its writer calls FUTEX_WAKE only then).
struct word {
    _Alignas(64) uint32_t value;
    uint32_t sleeping;
};

struct chan {
    struct word req, resp;
    uint64_t samples[];
};

static void post(struct word *w, uint32_t v) {
    __atomic_store_n(&w->value, v, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(&w->sleeping, __ATOMIC_SEQ_CST))
        futex(&w->value, FUTEX_WAKE, 1);
}

static void await_change(struct word *w, uint32_t old, long spin) {
    for (long i = 0; i < spin; i++) {
        if (__atomic_load_n(&w->value, __ATOMIC_ACQUIRE) != old)
            return;
        relax();
    }
    for (;;) {
        __atomic_store_n(&w->sleeping, 1, __ATOMIC_SEQ_CST);
        if (__atomic_load_n(&w->value, __ATOMIC_SEQ_CST) != old)
            break;
        futex(&w->value, FUTEX_WAIT, old);
    }
    __atomic_store_n(&w->sleeping, 0, __ATOMIC_SEQ_CST);
}

static int install_filter(unsigned int action, unsigned int flags) {
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, GUEST_NR, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, action),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = {.len = sizeof f / sizeof f[0], .filter = f};
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))
        return -1;
    return (int)syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &prog);
}

static void set_return(void *ctx, uint64_t v) {
    ucontext_t *uc = ctx;
#if defined(__x86_64__)
    uc->uc_mcontext.gregs[REG_RAX] = (greg_t)v;
#elif defined(__aarch64__)
    uc->uc_mcontext.regs[0] = v;
#endif
}

static int wait_child(pid_t pid) {
    int status;
    while (waitpid(pid, &status, 0) < 0)
        if (errno != EINTR)
            return -1;
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

// --- native: a real system call, with and without a filter in front ------------------

static void t_native(void) {
    uint64_t *s = calloc(n, sizeof *s);
    pin(cpu_a);
    for (size_t i = 0; i < warm; i++)
        syscall(SYS_getppid);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        syscall(SYS_getppid);
        s[i] = now_ns() - t;
    }
    report("native", "getppid, no filter", s, n);
    if (install_filter(SECCOMP_RET_TRAP, 0)) {
        note("native", "getppid, filter", strerror(errno));
        return;
    }
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        syscall(SYS_getppid);
        s[i] = now_ns() - t;
    }
    report("native", "getppid, filter allows it", s, n);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        s[i] = now_ns() - t;
    }
    report("native", "clock read alone", s, n);
}

// --- sigsys: the trap alone, to a handler in the guest's own process ------------------

static void on_sigsys_return(int sig, siginfo_t *si, void *ctx) {
    (void)sig;
    (void)si;
    set_return(ctx, 0);
}

static void t_sigsys(void) {
    uint64_t *s = calloc(n, sizeof *s);
    pin(cpu_a);
    struct sigaction sa = {0};
    sa.sa_sigaction = on_sigsys_return;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSYS, &sa, NULL);
    if (install_filter(SECCOMP_RET_TRAP, 0)) {
        note("sigsys", "in-process handler", strerror(errno));
        return;
    }
    for (size_t i = 0; i < warm; i++)
        syscall(GUEST_NR);
    for (size_t i = 0; i < n; i++) {
        uint64_t t = now_ns();
        syscall(GUEST_NR);
        s[i] = now_ns() - t;
    }
    report("sigsys", "in-process handler", s, n);
}

// --- futex and handoff: a round trip to another process -------------------------------

static struct chan *g_chan;
static uint32_t g_seq;
static long g_spin;

static void serve(struct chan *c, size_t total, long spin) {
    for (uint32_t i = 1; i <= total; i++) {
        await_change(&c->req, i - 1, spin);
        post(&c->resp, i);
    }
}

static void on_sigsys_handoff(int sig, siginfo_t *si, void *ctx) {
    (void)sig;
    (void)si;
    uint32_t i = ++g_seq;
    post(&g_chan->req, i);
    await_change(&g_chan->resp, i - 1, g_spin);
    set_return(ctx, 0);
}

// The guest side: `trap` sends each request through SIGSYS, else it posts it itself.
static void guest_loop(struct chan *c, size_t total, long spin, int trap) {
    for (uint32_t i = 1; i <= total; i++) {
        uint64_t t = now_ns();
        if (trap) {
            syscall(GUEST_NR);
        } else {
            post(&c->req, i);
            await_change(&c->resp, i - 1, spin);
        }
        if (i > warm)
            c->samples[i - 1 - warm] = now_ns() - t;
    }
}

static void pair(const char *test, const char *variant, int guest_cpu, int host_cpu,
                 long spin, int trap) {
    size_t total = warm + n;
    struct chan *c = shared(sizeof *c + n * sizeof(uint64_t));
    g_chan = c;
    g_spin = spin;
    g_seq = 0;
    pid_t pid = fork();
    if (pid == 0) {
        pin(guest_cpu);
        if (trap) {
            struct sigaction sa = {0};
            sa.sa_sigaction = on_sigsys_handoff;
            sa.sa_flags = SA_SIGINFO;
            sigaction(SIGSYS, &sa, NULL);
            if (install_filter(SECCOMP_RET_TRAP, 0))
                _exit(2);
        }
        guest_loop(c, total, spin, trap);
        _exit(0);
    }
    pin(host_cpu);
    serve(c, total, spin);
    if (wait_child(pid))
        note(test, variant, "guest failed");
    else
        report(test, variant, c->samples, n);
    munmap(c, sizeof *c + n * sizeof(uint64_t));
}

static void t_futex(void) {
    pair("futex", "same CPU", cpu_a, cpu_a, 0, 0);
    if (cpu_b >= 0) {
        pair("futex", "other core", cpu_a, cpu_b, 0, 0);
        pair("futex", "other core, spinning", cpu_a, cpu_b, SPIN, 0);
    }
}

static void t_handoff(void) {
    pair("handoff", "SIGSYS + futex, same CPU", cpu_a, cpu_a, 0, 1);
    if (cpu_b >= 0) {
        pair("handoff", "SIGSYS + futex, other core", cpu_a, cpu_b, 0, 1);
        pair("handoff", "SIGSYS + futex, other core, spinning", cpu_a, cpu_b, SPIN, 1);
    }
}

// --- notify: SECCOMP_RET_USER_NOTIF, served by the parent -----------------------------

static int send_fd(int sock, int fd) {
    char buf[CMSG_SPACE(sizeof(int))] = {0};
    char byte = 0;
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = buf,
                       .msg_controllen = sizeof buf};
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(h), &fd, sizeof fd);
    return sendmsg(sock, &m, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
    char buf[CMSG_SPACE(sizeof(int))] = {0};
    char byte;
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = buf,
                       .msg_controllen = sizeof buf};
    if (recvmsg(sock, &m, 0) != 1)
        return -1;
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    if (!h || h->cmsg_type != SCM_RIGHTS)
        return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(h), sizeof fd);
    return fd;
}

static void notify(const char *variant, int guest_cpu, int host_cpu, int sync) {
    size_t total = warm + n;
    struct chan *c = shared(sizeof *c + n * sizeof(uint64_t));
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) {
        note("notify", variant, strerror(errno));
        return;
    }
    pid_t pid = fork();
    if (pid == 0) {
        pin(guest_cpu);
        int fd = install_filter(SECCOMP_RET_USER_NOTIF, SECCOMP_FILTER_FLAG_NEW_LISTENER);
        if (fd < 0 || send_fd(sv[1], fd))
            _exit(2);
        guest_loop(c, total, 0, 1);
        _exit(0);
    }
    pin(host_cpu);
    int fd = recv_fd(sv[0]);
    if (fd < 0) {
        note("notify", variant, "no listener");
        wait_child(pid);
        return;
    }
    if (sync && ioctl(fd, SECCOMP_IOCTL_NOTIF_SET_FLAGS, SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP)) {
        note("notify", variant, strerror(errno));
        kill(pid, SIGKILL);
        wait_child(pid);
        return;
    }
    for (size_t i = 0; i < total; i++) {
        struct seccomp_notif req;
        memset(&req, 0, sizeof req);
        while (ioctl(fd, SECCOMP_IOCTL_NOTIF_RECV, &req))
            if (errno != EINTR) {
                note("notify", variant, strerror(errno));
                kill(pid, SIGKILL);
                wait_child(pid);
                return;
            }
        struct seccomp_notif_resp resp = {.id = req.id, .val = 0, .error = 0, .flags = 0};
        ioctl(fd, SECCOMP_IOCTL_NOTIF_SEND, &resp);
    }
    if (wait_child(pid))
        note("notify", variant, "guest failed");
    else
        report("notify", variant, c->samples, n);
    close(fd);
    close(sv[0]);
    close(sv[1]);
}

// The filter's action is the notification itself: the guest blocks in the kernel until
// the parent answers, and no handler of the guest's runs.
static void t_notify(void) {
    notify("same CPU", cpu_a, cpu_a, 0);
    notify("same CPU, sync wake-up", cpu_a, cpu_a, 1);
    if (cpu_b >= 0) {
        notify("other core", cpu_a, cpu_b, 0);
        notify("other core, sync wake-up", cpu_a, cpu_b, 1);
    }
}

// --- sysemu: ptrace's PTRACE_SYSEMU, registers read and written at each stop ----------

struct regs {
#if defined(__x86_64__)
    unsigned long r[27];
#elif defined(__aarch64__)
    uint64_t r[34];
#endif
};

static long reg_nr(const struct regs *g) {
#if defined(__x86_64__)
    return (long)g->r[15]; // orig_rax
#elif defined(__aarch64__)
    return (long)g->r[8];
#endif
}

static void reg_set_return(struct regs *g, uint64_t v) {
#if defined(__x86_64__)
    g->r[10] = v; // rax
#elif defined(__aarch64__)
    g->r[0] = v;
#endif
}

static void sysemu(const char *variant, int guest_cpu, int host_cpu) {
    size_t total = warm + n;
    struct chan *c = shared(sizeof *c + n * sizeof(uint64_t));
    pid_t pid = fork();
    if (pid == 0) {
        pin(guest_cpu);
        if (ptrace(PTRACE_TRACEME, 0, NULL, NULL))
            _exit(2);
        raise(SIGSTOP);
        guest_loop(c, total, 0, 1);
        for (;;)
            pause();
    }
    pin(host_cpu);
    int status;
    waitpid(pid, &status, 0);
    ptrace(PTRACE_SETOPTIONS, pid, NULL, (void *)(long)PTRACE_O_EXITKILL);
    size_t done = 0;
    const char *failed = NULL;
    for (;;) {
        if (ptrace(PTRACE_SYSEMU, pid, NULL, NULL)) {
            failed = strerror(errno);
            break;
        }
        if (waitpid(pid, &status, 0) < 0 || !WIFSTOPPED(status)) {
            failed = "guest left";
            break;
        }
        struct regs g;
        struct iovec iov = {.iov_base = &g, .iov_len = sizeof g};
        if (ptrace(PTRACE_GETREGSET, pid, (void *)NT_PRSTATUS, &iov)) {
            failed = strerror(errno);
            break;
        }
        if (reg_nr(&g) != GUEST_NR) {
            if (done != total)
                failed = "a system call other than the guest's (clock_gettime off the vDSO?)";
            break;
        }
        reg_set_return(&g, 0);
        ptrace(PTRACE_SETREGSET, pid, (void *)NT_PRSTATUS, &iov);
        done++;
    }
    kill(pid, SIGKILL);
    waitpid(pid, &status, 0);
    if (failed)
        note("sysemu", variant, failed);
    else
        report("sysemu", variant, c->samples, n);
}

static void t_sysemu(void) {
    sysemu("registers read and written, same CPU", cpu_a, cpu_a);
    if (cpu_b >= 0)
        sysemu("registers read and written, other core", cpu_a, cpu_b);
}

// --- fault: a guest page fault served in the guest's own process ----------------------

static int g_memfd = -1;
static char *g_fault_page;
static int g_map_on_fault;
static off_t g_fault_off;

static void on_sigsegv(int sig, siginfo_t *si, void *ctx) {
    (void)sig;
    (void)ctx;
    char *page = (char *)((uintptr_t)si->si_addr & ~(PAGE - 1));
    if (page != g_fault_page)
        abort();
    if (g_map_on_fault)
        mmap(page, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, g_memfd, g_fault_off);
    else
        mprotect(page, PAGE, PROT_READ | PROT_WRITE);
}

static void t_fault(void) {
    uint64_t *s = calloc(n, sizeof *s);
    pin(cpu_a);
    struct sigaction sa = {0};
    sa.sa_sigaction = on_sigsegv;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);

    char *p = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 1;
    g_fault_page = p;
    for (size_t i = 0; i < warm + n; i++) {
        mprotect(p, PAGE, PROT_NONE);
        uint64_t t = now_ns();
        ((volatile char *)p)[0] = 2;
        if (i >= warm)
            s[i - warm] = now_ns() - t;
    }
    report("fault", "SIGSEGV, handler mprotects the page", s, n);

    // Map-on-fault: a hole, filled by a shared page of guest memory (memfd) at the fault.
    size_t pages = 512;
    g_memfd = memfd_create("guest", 0);
    if (g_memfd < 0 || ftruncate(g_memfd, (off_t)(pages * PAGE))) {
        note("fault", "SIGSEGV, handler maps a guest page", strerror(errno));
        return;
    }
    char *lin = mmap(NULL, pages * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, g_memfd, 0);
    for (size_t i = 0; i < pages; i++)
        lin[i * PAGE] = 1;
    munmap(lin, pages * PAGE);
    g_map_on_fault = 1;
    for (size_t i = 0; i < warm + n; i++) {
        munmap(p, PAGE);
        g_fault_off = (off_t)((i * 7) % pages * PAGE);
        uint64_t t = now_ns();
        ((volatile char *)p)[0] = 3;
        if (i >= warm)
            s[i - warm] = now_ns() - t;
    }
    report("fault", "SIGSEGV, handler maps a guest page (memfd)", s, n);
}

// --- maps: guest pages mapped one by one at non-adjacent offsets ----------------------

static long read_long(const char *path) {
    FILE *f = fopen(path, "r");
    long v = -1;
    if (f) {
        if (fscanf(f, "%ld", &v) != 1)
            v = -1;
        fclose(f);
    }
    return v;
}

static size_t count_maps(void) {
    FILE *f = fopen("/proc/self/maps", "r");
    size_t lines = 0;
    int ch;
    if (!f)
        return 0;
    while ((ch = fgetc(f)) != EOF)
        lines += ch == '\n';
    fclose(f);
    return lines;
}

static void t_maps(void) {
    pin(cpu_a);
    long limit = read_long("/proc/sys/vm/max_map_count");
    size_t cap = 262144 + 1024;
    size_t pages = limit > 0 && (size_t)limit + 1024 < cap ? (size_t)limit + 1024 : cap;
    size_t before = count_maps();
    int fd = memfd_create("guest", 0);
    if (fd < 0 || ftruncate(fd, (off_t)(pages * PAGE))) {
        note("maps", "4 KiB pages", strerror(errno));
        return;
    }
    char *lin = mmap(NULL, pages * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    for (size_t i = 0; i < pages; i++)
        lin[i * PAGE] = 1;
    munmap(lin, pages * PAGE);

    char *r = mmap(NULL, pages * PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
                   -1, 0);
    uint64_t *s = calloc(pages, sizeof *s);
    size_t stride = 7919, mapped = 0;
    int err = 0;
    for (; mapped < pages; mapped++) {
        off_t off = (off_t)((mapped * stride) % pages * PAGE);
        uint64_t t = now_ns();
        void *m = mmap(r + mapped * PAGE, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED,
                       fd, off);
        s[mapped] = now_ns() - t;
        if (m == MAP_FAILED) {
            err = errno;
            break;
        }
    }
    char what[160];
    snprintf(what, sizeof what, "%zu mapped of %zu; max_map_count %ld, %zu maps before; %s",
             mapped, pages, limit, before, err ? strerror(err) : "no failure");
    note("maps", "4 KiB pages at non-adjacent offsets", what);
    size_t head = mapped < 1000 ? mapped : 1000;
    uint64_t *tmp = calloc(pages, sizeof *tmp);
    memcpy(tmp, s, head * sizeof *s);
    report("maps", "map one page, first 1000", tmp, head);
    memcpy(tmp, s + (mapped - head), head * sizeof *s);
    report("maps", "map one page, last 1000", tmp, head);
    memcpy(tmp, s, mapped * sizeof *s);
    report("maps", "map one page, all", tmp, mapped);
    for (size_t i = 0; i < mapped; i++) {
        uint64_t t = now_ns();
        ((volatile char *)r)[i * PAGE] = 2;
        tmp[i] = now_ns() - t;
    }
    report("maps", "first touch of a mapped page", tmp, mapped);
    for (size_t i = 0; i < mapped; i++) {
        uint64_t t = now_ns();
        munmap(r + i * PAGE, PAGE);
        tmp[i] = now_ns() - t;
    }
    report("maps", "unmap one page", tmp, mapped);
    munmap(r, pages * PAGE);

    // The same memory in 2 MiB units, also at non-adjacent offsets.
    size_t units = pages * PAGE / HUGE;
    char *base = mmap(NULL, (units + 1) * HUGE, PROT_NONE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    char *r2 = (char *)(((uintptr_t)base + HUGE - 1) & ~(HUGE - 1));
    size_t ustride = 7;
    while (units % ustride == 0)
        ustride += 2;
    for (size_t i = 0; i < units; i++) {
        off_t off = (off_t)((i * ustride) % units * HUGE);
        uint64_t t = now_ns();
        mmap(r2 + i * HUGE, HUGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd, off);
        tmp[i] = now_ns() - t;
    }
    report("maps", "map one 2 MiB unit", tmp, units);
    for (size_t i = 0; i < units; i++) {
        uint64_t t = now_ns();
        for (size_t k = 0; k < HUGE / PAGE; k++)
            ((volatile char *)r2)[i * HUGE + k * PAGE] = 3;
        tmp[i] = now_ns() - t;
    }
    report("maps", "first touch of a 2 MiB unit's 512 pages", tmp, units);
    for (size_t i = 0; i < units; i++) {
        uint64_t t = now_ns();
        munmap(r2 + i * HUGE, HUGE);
        tmp[i] = now_ns() - t;
    }
    report("maps", "unmap one 2 MiB unit", tmp, units);
}

// --- the host ------------------------------------------------------------------------

static int parse_list(const char *path, cpu_set_t *set) {
    FILE *f = fopen(path, "r");
    if (!f)
        return -1;
    CPU_ZERO(set);
    int a, b;
    char sep;
    while (fscanf(f, "%d", &a) == 1) {
        b = a;
        sep = (char)fgetc(f);
        if (sep == '-') {
            if (fscanf(f, "%d", &b) != 1)
                break;
            sep = (char)fgetc(f);
        }
        for (int c = a; c <= b && c < CPU_SETSIZE; c++)
            CPU_SET(c, set);
        if (sep != ',')
            break;
    }
    fclose(f);
    return 0;
}

static void choose_cpus(void) {
    cpu_set_t allowed, siblings;
    if (sched_getaffinity(0, sizeof allowed, &allowed))
        return;
    for (int c = 0; c < CPU_SETSIZE; c++)
        if (CPU_ISSET(c, &allowed)) {
            cpu_a = c;
            break;
        }
    char path[96];
    snprintf(path, sizeof path, "/sys/devices/system/cpu/cpu%d/topology/thread_siblings_list",
             cpu_a);
    if (parse_list(path, &siblings)) {
        CPU_ZERO(&siblings);
        CPU_SET(cpu_a, &siblings);
    }
    for (int c = 0; c < CPU_SETSIZE; c++)
        if (CPU_ISSET(c, &allowed) && !CPU_ISSET(c, &siblings)) {
            cpu_b = c;
            break;
        }
}

static void print_file(const char *label, const char *path) {
    char buf[256] = {0};
    FILE *f = fopen(path, "r");
    if (!f) {
        printf("- %s: (none)\n", label);
        return;
    }
    if (!fgets(buf, sizeof buf, f))
        buf[0] = 0;
    fclose(f);
    buf[strcspn(buf, "\n")] = 0;
    printf("- %s: %s\n", label, buf);
}

static void print_host(void) {
    struct utsname u;
    uname(&u);
    printf("- kernel: %s %s %s\n", u.sysname, u.release, u.machine);
    FILE *f = fopen("/proc/cpuinfo", "r");
    char line[512];
    int model = 0, hyper = 0, implementer = 0, part = 0;
    while (f && fgets(line, sizeof line, f)) {
        if (!model && !strncmp(line, "model name", 10)) {
            printf("- cpu: %s", strchr(line, ':') + 2);
            model = 1;
        }
        if (!implementer && !strncmp(line, "CPU implementer", 15)) {
            printf("- cpu implementer: %s", strchr(line, ':') + 2);
            implementer = 1;
        }
        if (!part && !strncmp(line, "CPU part", 8)) {
            printf("- cpu part: %s", strchr(line, ':') + 2);
            part = 1;
        }
        if (!strncmp(line, "flags", 5) && strstr(line, " hypervisor"))
            hyper = 1;
    }
    if (f)
        fclose(f);
    printf("- x86 hypervisor flag: %s\n", hyper ? "yes" : "no");
    print_file("dmi vendor", "/sys/class/dmi/id/sys_vendor");
    print_file("dmi product", "/sys/class/dmi/id/product_name");
    printf("- /dev/kvm: %s\n", access("/dev/kvm", F_OK) ? "absent" : "present");
    printf("- cpus online: %ld; measured on cpu %d, other core cpu %d\n",
           sysconf(_SC_NPROCESSORS_ONLN), cpu_a, cpu_b);
    print_file("clocksource", "/sys/devices/system/clocksource/clocksource0/current_clocksource");
    print_file("vm.max_map_count", "/proc/sys/vm/max_map_count");
    print_file("vm.unprivileged_userfaultfd", "/proc/sys/vm/unprivileged_userfaultfd");
    print_file("shmem THP", "/sys/kernel/mm/transparent_hugepage/shmem_enabled");
    print_file("yama ptrace_scope", "/proc/sys/kernel/yama/ptrace_scope");
    DIR *d = opendir("/sys/devices/system/cpu/vulnerabilities");
    struct dirent *e;
    while (d && (e = readdir(d))) {
        if (e->d_name[0] == '.')
            continue;
        char path[300];
        snprintf(path, sizeof path, "/sys/devices/system/cpu/vulnerabilities/%s", e->d_name);
        char label[300];
        snprintf(label, sizeof label, "vulnerability %s", e->d_name);
        print_file(label, path);
    }
    if (d)
        closedir(d);
}

struct test {
    const char *name;
    void (*run)(void);
};

static const struct test tests[] = {
    {"native", t_native}, {"sigsys", t_sigsys}, {"futex", t_futex},
    {"handoff", t_handoff}, {"notify", t_notify}, {"sysemu", t_sysemu},
    {"fault", t_fault}, {"maps", t_maps},
};

int main(int argc, char **argv) {
    int first = 1;
    if (argc > 2 && !strcmp(argv[1], "-n")) {
        n = strtoul(argv[2], NULL, 10);
        first = 3;
    }
    if (n == 0)
        return 2;
    choose_cpus();
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
        // Each test in a child of its own: filters and pins stay with it.
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

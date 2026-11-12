// softvirt-baselines: one workload, run natively, under gVisor (systrap), under
// User-mode Linux (seccomp mode) and in a KVM guest, so the costs a software
// hypervisor adds to a guest's system calls, processes, faults and context switches can
// be compared on one host. Each mode times one operation per iteration and prints n,
// p50, p90, p99 and max in nanoseconds. Static, so it runs as a guest's init too: as
// PID 1 it mounts what it needs, runs every mode and powers the machine off.
// Method and results: docs/research/platform-measurements.md (the softvirt entries).
//
//   cc -O2 -static -o workload workload.c && ./workload [-n N] [mode...]
//   modes: syscall forkexec fault pipe compute (all when none is named)

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/reboot.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static size_t n = 20000;
static const size_t warm = 200;
static char self[4096];

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static uint64_t pct(const uint64_t *s, size_t len, unsigned p) {
    size_t rank = (p * len + 99) / 100;
    return s[rank ? rank - 1 : 0];
}

static void report(const char *mode, const char *what, uint64_t *s, size_t len) {
    if (len == 0) {
        printf("| %s | %s | 0 | | | | |\n", mode, what);
        fflush(stdout);
        return;
    }
    qsort(s, len, sizeof *s, cmp_u64);
    printf("| %s | %s | %zu | %llu | %llu | %llu | %llu |\n", mode, what, len,
           (unsigned long long)pct(s, len, 50), (unsigned long long)pct(s, len, 90),
           (unsigned long long)pct(s, len, 99), (unsigned long long)s[len - 1]);
    fflush(stdout);
}

static void m_syscall(uint64_t *s) {
    for (size_t i = 0; i < warm + n; i++) {
        uint64_t t = now_ns();
        syscall(SYS_getppid);
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[i - warm] = d;
    }
    report("syscall", "getppid", s, n);
}

// A process made and gone: fork, exec this program to exit at once, wait.
static void m_forkexec(uint64_t *s) {
    size_t count = n / 20 ? n / 20 : 1;
    for (size_t i = 0; i < warm / 20 + count; i++) {
        uint64_t t = now_ns();
        pid_t pid = fork();
        if (pid == 0) {
            char *argv[] = {self, "--exit", NULL};
            execv(self, argv);
            _exit(127);
        }
        int status;
        waitpid(pid, &status, 0);
        uint64_t d = now_ns() - t;
        if (i >= warm / 20)
            s[i - warm / 20] = d;
    }
    report("forkexec", "fork, exec, exit, wait", s, count);
}

// Sixteen fresh pages touched, the way a program's heap grows: 16 first-touch faults.
static void m_fault(uint64_t *s) {
    size_t len = 16 * 4096;
    for (size_t i = 0; i < warm + n; i++) {
        char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        uint64_t t = now_ns();
        for (size_t k = 0; k < len; k += 4096)
            p[k] = 1;
        uint64_t d = now_ns() - t;
        munmap(p, len);
        if (i >= warm)
            s[i - warm] = d;
    }
    report("fault", "16 first-touch page faults", s, n);
}

// One byte to another process and back through two pipes: two context switches.
static void m_pipe(uint64_t *s) {
    int there[2], back[2];
    if (pipe(there) || pipe(back))
        return;
    pid_t pid = fork();
    if (pid == 0) {
        // Each side holds only its own ends, so this one sees EOF when the other closes.
        close(there[1]);
        close(back[0]);
        char c;
        while (read(there[0], &c, 1) == 1)
            if (write(back[1], &c, 1) != 1)
                break;
        _exit(0);
    }
    close(there[0]);
    close(back[1]);
    char c = 'x';
    size_t done = 0;
    for (size_t i = 0; i < warm + n; i++) {
        uint64_t t = now_ns();
        if (write(there[1], &c, 1) != 1 || read(back[0], &c, 1) != 1)
            break;
        uint64_t d = now_ns() - t;
        if (i >= warm)
            s[done++] = d;
    }
    close(there[1]);
    close(back[0]);
    waitpid(pid, NULL, 0);
    report("pipe", "one byte there and back", s, done);
}

// User code alone, no kernel entry: FNV-1a over 256 KiB (an L2-sized buffer), a load, an
// xor and a multiply a byte. What a translator costs a guest's own instructions and
// memory accesses, which hardware virtualization runs untouched.
static void m_compute(uint64_t *s) {
    size_t len = 256 << 10, count = n / 20 ? n / 20 : 1;
    unsigned char *buf = malloc(len);
    if (!buf)
        return;
    for (size_t i = 0; i < len; i++)
        buf[i] = (unsigned char)(i * 131);
    volatile uint64_t sink = 0;
    for (size_t i = 0; i < warm / 20 + count; i++) {
        uint64_t t = now_ns();
        uint64_t h = 1469598103934665603ULL;
        for (size_t k = 0; k < len; k++)
            h = (h ^ buf[k]) * 1099511628211ULL;
        sink += h;
        uint64_t d = now_ns() - t;
        if (i >= warm / 20)
            s[i - warm / 20] = d;
    }
    free(buf);
    report("compute", "FNV-1a over 256 KiB", s, count);
}

struct mode {
    const char *name;
    void (*run)(uint64_t *);
};

static const struct mode modes[] = {
    {"syscall", m_syscall}, {"forkexec", m_forkexec}, {"fault", m_fault}, {"pipe", m_pipe},
    {"compute", m_compute},
};

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--exit"))
        return 0;
    int as_init = getpid() == 1;
    if (as_init) {
        mkdir("/proc", 0755);
        mount("proc", "/proc", "proc", 0, NULL);
    }
    ssize_t len = readlink("/proc/self/exe", self, sizeof self - 1);
    if (len <= 0)
        return 2;
    self[len] = 0;
    int first = 1;
    if (argc > 2 && !strcmp(argv[1], "-n")) {
        n = strtoul(argv[2], NULL, 10);
        first = 3;
    }
    uint64_t *s = calloc(n + warm, sizeof *s);
    struct utsname u;
    uname(&u);
    printf("- kernel: %s %s %s%s\n", u.sysname, u.release, u.machine, as_init ? " (as init)" : "");
    printf("\n| mode | operation | n | p50 ns | p90 ns | p99 ns | max ns |\n");
    printf("|---|---|---|---|---|---|---|\n");
    fflush(stdout);
    for (size_t m = 0; m < sizeof modes / sizeof modes[0]; m++) {
        int chosen = first >= argc;
        for (int a = first; a < argc; a++)
            chosen |= !strcmp(argv[a], modes[m].name);
        if (chosen)
            modes[m].run(s);
    }
    if (as_init) {
        // User-mode Linux ends at a power-off; a Firecracker guest at a restart
        // (reboot=k), which a kernel command line naming softvirt.restart asks for.
        char cmdline[1024] = {0};
        int fd = open("/proc/cmdline", O_RDONLY);
        if (fd >= 0) {
            if (read(fd, cmdline, sizeof cmdline - 1) < 0)
                cmdline[0] = 0;
            close(fd);
        }
        sync();
        reboot(strstr(cmdline, "softvirt.restart") ? LINUX_REBOOT_CMD_RESTART
                                                    : LINUX_REBOOT_CMD_POWER_OFF);
    }
    return 0;
}

// How finely proc_pid_rusage(2) (RUSAGE_INFO_V0) shows another process's CPU on macOS, and
// how finely clock_gettime(2) of its CPU clock does on Linux: a child spins (mode "spin") or
// wakes on each byte of a pipe, runs about 2 us and blocks again (mode "wake"); the parent
// reads its CPU back to back for 200 ms and prints, for every read that found growth, the
// wall time since the read before that did (the reading's step), and the cost of one read.
// cc -O2 rusage-steps.c -o rusage-steps && ./rusage-steps spin && ./rusage-steps wake
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <libproc.h>
#include <mach/mach_time.h>
static mach_timebase_info_data_t tb;
static uint64_t cpu_of(pid_t pid) {
    struct rusage_info_v0 ri;
    if (proc_pid_rusage(pid, RUSAGE_INFO_V0, (rusage_info_t *)&ri) != 0) return 0;
    return (ri.ri_user_time + ri.ri_system_time) * tb.numer / tb.denom;
}
#else
static uint64_t cpu_of(pid_t pid) {
    clockid_t c; struct timespec t;
    if (clock_getcpuclockid(pid, &c) || clock_gettime(c, &t)) return 0;
    return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}
#endif
static uint64_t now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }
static int cmp(const void *a, const void *b) { uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b; return x < y ? -1 : x > y; }
static void pct(const char *what, uint64_t *v, size_t n) {
    if (!n) { printf("%s: n=0\n", what); return; }
    qsort(v, n, sizeof *v, cmp);
    printf("%s: n=%zu min=%.1fus p50=%.1fus p90=%.1fus p99=%.1fus max=%.1fus\n", what, n, v[0] / 1e3,
           v[n / 2] / 1e3, v[n * 9 / 10] / 1e3, v[n * 99 / 100] / 1e3, v[n - 1] / 1e3);
}
int main(int argc, char **argv) {
#ifdef __APPLE__
    mach_timebase_info(&tb);
#endif
    int wake = argc > 1 && !strcmp(argv[1], "wake");
    int p[2]; if (pipe(p)) return 1;
    pid_t pid = fork();
    if (pid == 0) {
        close(p[1]);
        if (!wake) for (;;) __asm__ volatile("");
        char c; while (read(p[0], &c, 1) == 1) { uint64_t t = now(); while (now() - t < 2000) {} }
        _exit(0);
    }
    close(p[0]);
    usleep(20000);
    static uint64_t steps[1 << 20], costs[1 << 20], seen[1 << 16];
    size_t ns = 0, nc = 0, nw = 0;
    uint64_t start = now(), last_cpu = cpu_of(pid), last_grow = now();
    uint64_t poked = 0;
    while (now() - start < 200000000ull) {
        if (wake && !poked) { poked = now(); if (write(p[1], "x", 1) != 1) break; }
        uint64_t a = now(); uint64_t c = cpu_of(pid); uint64_t b = now();
        if (nc < (1 << 20)) costs[nc++] = b - a;
        if (c > last_cpu) {
            if (ns < (1 << 20)) steps[ns++] = a - last_grow;
            if (wake && poked && nw < (1 << 16)) { seen[nw++] = a - poked; poked = 0; usleep(1000); }
            last_cpu = c; last_grow = b;
        }
    }
    kill(pid, SIGKILL);
    printf("mode=%s\n", wake ? "wake" : "spin");
    pct("one read's cost", costs, nc);
    pct("wall between reads that found growth", steps, ns);
    if (wake) pct("from a byte written to the read that first showed its work", seen, nw);
    return 0;
}

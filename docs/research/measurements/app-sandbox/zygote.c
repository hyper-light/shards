// M75: what starting a sandboxed VM process costs by launch and by fork. A process in App
// Sandbox launches (posix_spawn of itself) or forks K children at once, each of which
// creates and destroys a Hypervisor.framework VM and tries to open OUTSIDE, a file
// outside its container, which it must not reach; the parent times each child from its
// start to its reaping. A forked child is the parent's copy: it was never launched.
//
//   zygote fork|spawn K ROUNDS OUTSIDE
#include <Hypervisor/Hypervisor.h>
#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

// Exit status: 0 VM made and OUTSIDE refused; 1 VM refused; 2 OUTSIDE reached.
static int child(const char *outside) {
    if (hv_vm_create(NULL) != HV_SUCCESS) return 1;
    hv_vm_destroy();
    int fd = open(outside, O_RDONLY);
    if (fd >= 0) return 2;
    return 0;
}

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "child")) return child(argv[2]);
    if (argc != 5) {
        fprintf(stderr, "usage: zygote fork|spawn K ROUNDS OUTSIDE\n");
        return 2;
    }
    int spawn = !strcmp(argv[1], "spawn"), k = atoi(argv[2]), rounds = atoi(argv[3]);
    const char *outside = argv[4];
    int n = k * rounds, bad = 0;
    double *lat = calloc(n, sizeof *lat);
    pid_t *pids = calloc(k, sizeof *pids);
    double *t0 = calloc(k, sizeof *t0);
    for (int r = 0; r < rounds; r++) {
        for (int i = 0; i < k; i++) {
            t0[i] = now_ms();
            if (spawn) {
                char *args[] = {argv[0], "child", (char *)outside, NULL};
                if (posix_spawn(&pids[i], argv[0], NULL, NULL, args, environ) != 0) return 3;
            } else {
                pids[i] = fork();
                if (pids[i] == 0) _exit(child(outside));
                if (pids[i] < 0) return 3;
            }
        }
        for (int i = 0; i < k; i++) {
            int st;
            waitpid(pids[i], &st, 0);
            lat[r * k + i] = now_ms() - t0[i];
            if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) bad++;
        }
    }
    qsort(lat, n, sizeof *lat, cmp);
    printf("%s %2d at once: n %d p50 %.2f p90 %.2f max %.2f ms; children failing %d\n", argv[1], k, n,
           lat[n / 2], lat[n * 9 / 10], lat[n - 1], bad);
    return 0;
}

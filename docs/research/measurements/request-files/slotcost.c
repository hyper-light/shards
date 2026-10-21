// M255 (docs/research/platform-measurements.md): what a daemon spends making and removing
// one request slot (D185): FIFOs made, both ends of each opened (as shards_ipc::slots's
// Requests::make), then closed and unlinked; with 5 FIFOs (a slot with streams) and with
// 2 (say and answer alone). Wall and CPU time per slot.
//
//   slotcost DIR N
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static double cpu(void) { struct rusage u; getrusage(RUSAGE_SELF, &u); return u.ru_utime.tv_sec + u.ru_stime.tv_sec + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) / 1e6; }
static int cmp(const void *a, const void *b) { double x = *(const double *)a, y = *(const double *)b; return x < y ? -1 : x > y; }
int main(int argc, char **argv) {
    const char *dir = argv[1]; int n = atoi(argv[2]);
    const char *names[] = {"say", "answer", "in", "out", "err"};
    for (int fifos = 5; fifos >= 2; fifos -= 3) {
        double *v = calloc(n, sizeof *v);
        double c0 = cpu();
        for (int i = 0; i < n; i++) {
            double t = now();
            int fds[10];
            char p[600];
            for (int k = 0; k < fifos; k++) {
                snprintf(p, sizeof p, "%s/%d.%s", dir, i, names[k]);
                mkfifo(p, 0600);
                fds[2 * k] = open(p, O_RDONLY | O_NONBLOCK);
                fds[2 * k + 1] = open(p, O_WRONLY);
            }
            for (int k = 0; k < 2 * fifos; k++) close(fds[k]);
            for (int k = 0; k < fifos; k++) { snprintf(p, sizeof p, "%s/%d.%s", dir, i, names[k]); unlink(p); }
            v[i] = (now() - t) * 1e6;
        }
        double spent = (cpu() - c0) * 1e6 / n;
        qsort(v, n, sizeof *v, cmp);
        printf("%d FIFOs: n=%d wall p50=%.0fus p90=%.0fus p99=%.0fus max=%.0fus; CPU %.0fus a slot\n", fifos, n, v[n / 2], v[n * 9 / 10], v[n * 99 / 100], v[n - 1], spent);
        free(v);
    }
    return 0;
}

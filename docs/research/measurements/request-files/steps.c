// M254 (docs/research/platform-measurements.md): each step of a made-ahead slot's request
// (D185), timed apart: open(lock), flock, open(answer), open(say), the request's write, and
// the answer's read; slots made ahead by a daemon process, as in requests.c, but named flat
// in one directory.
//
//   steps DIR N
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static double now_us(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e6 + t.tv_nsec / 1e3; }
static int cmp(const void *a, const void *b) { double x = *(const double *)a, y = *(const double *)b; return x < y ? -1 : x > y; }
int main(int argc, char **argv) {
    const char *dir = argv[1]; int n = atoi(argv[2]);
    char p[600];
    int ready[2]; pipe(ready);
    fflush(stdout);
    pid_t d = fork();
    if (d == 0) {
        int *say = calloc(n, sizeof *say), *ans = calloc(n, sizeof *ans);
        for (int i = 0; i < n; i++) {
            snprintf(p, sizeof p, "%s/%d.lock", dir, i); close(open(p, O_RDONLY | O_CREAT, 0600));
            snprintf(p, sizeof p, "%s/%d.say", dir, i); mkfifo(p, 0600);
            say[i] = open(p, O_RDONLY | O_NONBLOCK); open(p, O_WRONLY); fcntl(say[i], F_SETFL, 0);
            snprintf(p, sizeof p, "%s/%d.answer", dir, i); mkfifo(p, 0600);
            open(p, O_RDONLY | O_NONBLOCK); ans[i] = open(p, O_WRONLY);
        }
        write(ready[1], "r", 1);
        for (int i = 0; i < n; i++) { char req[64]; if (read(say[i], req, 64) != 64) _exit(1); write(ans[i], "k", 1); }
        _exit(0);
    }
    char c; read(ready[0], &c, 1);
    double *s[7]; for (int k = 0; k < 7; k++) s[k] = calloc(n, sizeof(double));
    for (int i = 0; i < n; i++) {
        double t0 = now_us();
        snprintf(p, sizeof p, "%s/%d.lock", dir, i); int l = open(p, O_RDONLY); double t1 = now_us();
        flock(l, LOCK_EX | LOCK_NB); double t2 = now_us();
        snprintf(p, sizeof p, "%s/%d.answer", dir, i); int a = open(p, O_RDONLY); double t3 = now_us();
        snprintf(p, sizeof p, "%s/%d.say", dir, i); int w = open(p, O_WRONLY); double t4 = now_us();
        char req[64] = {0}; write(w, req, 64); double t5 = now_us();
        read(a, &c, 1); double t6 = now_us();
        s[0][i] = t1 - t0; s[1][i] = t2 - t1; s[2][i] = t3 - t2; s[3][i] = t4 - t3; s[4][i] = t5 - t4; s[5][i] = t6 - t5; s[6][i] = t6 - t0;
        close(w); close(a); close(l);
    }
    waitpid(d, NULL, 0);
    const char *names[] = {"open lock", "flock", "open answer", "open say", "write", "read answer", "all"};
    for (int k = 0; k < 7; k++) { qsort(s[k], n, sizeof(double), cmp); printf("%-12s p50=%.1fus p90=%.1fus p99=%.1fus\n", names[k], s[k][n/2], s[k][n*9/10], s[k][n*99/100]); }
    return 0;
}

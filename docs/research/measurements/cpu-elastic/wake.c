// What waking another vCPU costs a guest (D191): round trips through two pipes between a
// thread on CPU 0 and one on CPU X, for each X given, n round trips each, printing the round
// trip's p50, p90, p99 and max in us; then n/10 fork+exec+waits of /bin/true, the child on
// the parent's CPU and then on any. Built static for the guest's architecture by wake.sh.
//     wake N CPU...
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static int ab[2], ba[2], n;
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e6 + t.tv_nsec / 1e3; }
static void pin(int cpu) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s); if (sched_setaffinity(0, sizeof s, &s)) { perror("affinity"); exit(1); } }
static void *echo(void *arg) { pin(*(int *)arg); char c; for (int i = 0; i < n; i++) { if (read(ab[0], &c, 1) != 1 || write(ba[1], &c, 1) != 1) exit(1); } return 0; }
static int cmp(const void *a, const void *b) { double x = *(double *)a, y = *(double *)b; return x < y ? -1 : x > y; }
static void report(const char *what, double *t) { qsort(t, n, sizeof *t, cmp); printf("%s: p50 %.1f p90 %.1f p99 %.1f max %.1f us\n", what, t[n / 2], t[n * 9 / 10], t[n * 99 / 100], t[n - 1]); }
int main(int argc, char **argv) {
  n = argc > 1 ? atoi(argv[1]) : 2000;
  double *t = calloc(n, sizeof *t);
  pin(0);
  for (int a = 2; a < argc; a++) {
    int x = atoi(argv[a]);
    if (pipe(ab) || pipe(ba)) return 1;
    pthread_t th; pthread_create(&th, 0, echo, &x);
    char c = 1;
    for (int i = 0; i < n; i++) { double t0 = now(); if (write(ab[1], &c, 1) != 1 || read(ba[0], &c, 1) != 1) return 1; t[i] = now() - t0; }
    pthread_join(th, 0); close(ab[0]); close(ab[1]); close(ba[0]); close(ba[1]);
    char what[64]; snprintf(what, sizeof what, "round trip, CPU 0 and %d", x); report(what, t);
  }
  for (int pinned = 1; pinned >= 0; pinned--) {
    cpu_set_t all; CPU_ZERO(&all); for (int c = 0; c < CPU_SETSIZE; c++) CPU_SET(c, &all);
    if (!pinned) sched_setaffinity(0, sizeof all, &all); else pin(0);
    for (int i = 0; i < n / 10; i++) {
      double t0 = now(); pid_t p = fork();
      if (p == 0) { execl("/bin/true", "true", (char *)0); _exit(127); }
      int st; waitpid(p, &st, 0); t[i] = now() - t0;
    }
    int m = n; n = n / 10; report(pinned ? "fork+exec+wait, all on CPU 0" : "fork+exec+wait, on any CPU", t); n = m;
  }
  return 0;
}

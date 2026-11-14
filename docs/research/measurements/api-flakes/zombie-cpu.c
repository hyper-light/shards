/* PM M584: does a zombie's process CPU clock still grow after waitid(WEXITED|WNOWAIT) sees it?
 * cc -O2 -o zombie-cpu zombie-cpu.c && ./zombie-cpu 5000 */
#define _GNU_SOURCE
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
extern char **environ;
static long long cpu(pid_t pid) {
  clockid_t c; struct timespec t;
  if (clock_getcpuclockid(pid, &c)) return -1;
  if (clock_gettime(c, &t)) return -2;
  return t.tv_sec * 1000000000LL + t.tv_nsec;
}
int main(int argc, char **argv) {
  int n = argc > 1 ? atoi(argv[1]) : 2000, moved = 0;
  long long maxd = 0;
  for (int i = 0; i < n; i++) {
    pid_t pid; char *a[] = {"true", NULL};
    if (posix_spawnp(&pid, "true", NULL, NULL, a, environ)) return 1;
    siginfo_t si;
    if (waitid(P_PID, pid, &si, WEXITED | WNOWAIT)) return 2;
    long long c1 = cpu(pid), c2 = cpu(pid), c3 = cpu(pid);
    usleep(200);
    long long c4 = cpu(pid);
    if (c1 != c4) { moved++; if (c4 - c1 > maxd) maxd = c4 - c1;
      if (moved <= 5) printf("moved: %lld %lld %lld %lld (+%lld ns)\n", c1, c2, c3, c4, c4 - c1); }
    waitpid(pid, NULL, 0);
  }
  printf("n=%d moved=%d max=%lldns\n", n, moved, maxd);
  return 0;
}

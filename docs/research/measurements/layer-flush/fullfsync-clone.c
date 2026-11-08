// F_FULLFSYNC after small writes through a shared mapping: a fresh sparse file against a
// clone of one that holds data (clonefile(2)), as a restore's layer is its template's clone.
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/clonefile.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static void run(const char *name, int fd, size_t len) {
  char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (m == MAP_FAILED) { perror("mmap"); exit(1); }
  double xs[20];
  for (int i = 0; i < 20; i++) {
    // A journal's worth: three 4 KiB blocks, as a small commit writes.
    for (int b = 0; b < 3; b++) m[(size_t)(i * 3 + b) * 65536 + 4096] = (char)(i + 1);
    double t0 = now(); fcntl(fd, F_FULLFSYNC); xs[i] = now() - t0;
  }
  for (int i = 0; i < 20; i++) for (int j = i + 1; j < 20; j++) if (xs[j] < xs[i]) { double t = xs[i]; xs[i] = xs[j]; xs[j] = t; }
  printf("%-6s F_FULLFSYNC p50 %.2f ms p90 %.2f ms max %.2f ms\n", name, xs[10], xs[18], xs[19]);
  munmap(m, len);
}
int main(int argc, char **argv) {
  size_t len = (size_t)1 << 30;
  char a[1024], b[1024]; snprintf(a, sizeof a, "%s/fresh", argv[1]); snprintf(b, sizeof b, "%s/clone", argv[1]);
  unlink(a); unlink(b);
  int fd = open(a, O_RDWR | O_CREAT | O_TRUNC, 0600); ftruncate(fd, (off_t)len);
  // Data in the region the writes go to, then synced, as a template's layer holds.
  char *buf = calloc(1, 8 << 20); for (int i = 0; i < 8 << 20; i++) buf[i] = (char)i;
  pwrite(fd, buf, 8 << 20, 0); fcntl(fd, F_FULLFSYNC); close(fd);
  if (clonefile(a, b, 0) != 0) { perror("clonefile"); return 1; }
  int fresh = open(a, O_RDWR), clone = open(b, O_RDWR);
  run("fresh", fresh, len); run("clone", clone, len);
  close(fresh); close(clone); unlink(a); unlink(b);
  return 0;
}

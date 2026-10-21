// Whether F_FULLFSYNC alone writes back pages dirtied through a shared mapping on macOS
// (mincore's MINCORE_MODIFIED), as plain fsync does not; and msync's cost by mapping size.
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
int main(int argc, char **argv) {
  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  for (int gib = 1; gib <= 16; gib *= 4) {
    size_t len = (size_t)gib << 30;
    int fd = open(argv[1], O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0 || ftruncate(fd, (off_t)len) != 0) { perror("file"); return 1; }
    char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) { perror("mmap"); return 1; }
    for (int round = 0; round < 3; round++) {
      m[0] = (char)(round + 1); m[len / 2] = 1;
      char v = 0; mincore(m, page, &v); int before = (v & MINCORE_MODIFIED) != 0;
      double t0 = now(); fcntl(fd, F_FULLFSYNC); double t1 = now();
      mincore(m, page, &v); int after = (v & MINCORE_MODIFIED) != 0;
      double t2 = now(); msync(m, len, MS_SYNC); double t3 = now();
      printf("%2d GiB round %d: modified %d, after F_FULLFSYNC %d (%.3f ms); msync then %.3f ms\n", gib, round, before, after, t1 - t0, t3 - t2);
    }
    munmap(m, len); close(fd); unlink(argv[1]);
  }
  return 0;
}

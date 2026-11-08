// What a layer's flush costs on this host: a sparse file the size of a layer mapped shared
// and writable, a few pages dirtied through the mapping, then msync(MS_SYNC), fsync and
// F_FULLFSYNC timed, and whether fsync alone writes back what the mapping dirtied (mincore's
// MINCORE_MODIFIED on macOS).
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static int modified(char *p, size_t page) {
#ifdef __APPLE__
  char v = 0; if (mincore(p, page, &v) != 0) return -1; return (v & MINCORE_MODIFIED) != 0;
#else
  (void)p; (void)page; return -1;
#endif
}
int main(int argc, char **argv) {
  if (argc < 4) { fprintf(stderr, "usage: probe FILE GIB DIRTY_PAGES\n"); return 2; }
  size_t len = (size_t)atol(argv[2]) << 30; int dirty = atoi(argv[3]);
  size_t page = (size_t)sysconf(_SC_PAGESIZE);
  int fd = open(argv[1], O_RDWR | O_CREAT | O_TRUNC, 0600);
  if (fd < 0 || ftruncate(fd, (off_t)len) != 0) { perror("file"); return 1; }
  char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (m == MAP_FAILED) { perror("mmap"); return 1; }
  for (int round = 0; round < 5; round++) {
    for (int i = 0; i < dirty; i++) m[(size_t)i * (len / dirty)] = (char)(round + 1);
    int before = modified(m, page);
    double t0 = now(); fsync(fd); double t1 = now();
    int after_fsync = modified(m, page);
    for (int i = 0; i < dirty; i++) m[(size_t)i * (len / dirty) + 1] = (char)(round + 2);
    double t2 = now(); msync(m, len, MS_SYNC); double t3 = now();
#ifdef __APPLE__
    double t4 = now(); fcntl(fd, F_FULLFSYNC); double t5 = now();
#else
    double t4 = now(); fdatasync(fd); double t5 = now();
#endif
    double t6 = now(); msync(m, len, MS_SYNC); double t7 = now();
    printf("round %d: dirty %d, modified before %d after fsync %d; fsync %.3f ms, msync %.3f ms, full %.3f ms, clean msync %.3f ms\n",
           round, dirty, before, after_fsync, t1 - t0, t3 - t2, t5 - t4, t7 - t6);
  }
  munmap(m, len); close(fd); unlink(argv[1]);
  return 0;
}

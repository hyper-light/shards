// Bytes the C library allocates per open directory stream: malloc's in-use bytes
// before and after N opendir(3) calls of one directory (fdopendir of fresh openat ".").
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#ifdef __APPLE__
#include <malloc/malloc.h>
static size_t in_use(void) {
  malloc_statistics_t s;
  malloc_zone_statistics(NULL, &s);
  return s.size_in_use;
}
#else
#include <malloc.h>
static size_t in_use(void) {
  struct mallinfo2 m = mallinfo2();
  return m.uordblks + m.hblkhd;
}
#endif

int main(int argc, char **argv) {
  const char *path = argc > 1 ? argv[1] : ".";
  enum { N = 200 };
  static DIR *dirs[N];
  int dir = open(path, O_RDONLY | O_DIRECTORY);
  size_t before = in_use();
  for (int i = 0; i < N; i++) {
    int fd = openat(dir, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    dirs[i] = fdopendir(fd);
    if (!dirs[i]) return 1;
    readdir(dirs[i]);  // the first read fills the buffer
  }
  size_t after = in_use();
  printf("%d streams: %zu bytes in use more, %zu per stream\n", N, after - before, (after - before) / N);
  return 0;
}

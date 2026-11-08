/* How many bytes one pread or pwrite may ask for (PM M35).
 *
 * For INT_MAX and INT_MAX + 1 bytes, from memory that is mapped but never touched:
 * pwrite to /dev/null, which takes any count without reading it, and pread from an
 * empty file, which has nothing to give. What each call returns shows whether the
 * kernel took the count, cut it, or refused it.
 *
 *   cc -O1 -o probe probe.c && ./probe
 *   zig cc -target aarch64-linux-musl -O1 -o probe probe.c   (then run it on Linux)
 */
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static void report(const char *call, size_t len, ssize_t n) {
  if (n < 0)
    printf("%-22s %10zu bytes: %s\n", call, len, strerror(errno));
  else
    printf("%-22s %10zu bytes: %zd\n", call, len, n);
}

int main(void) {
  size_t lens[] = {(size_t)INT_MAX, (size_t)INT_MAX + 1};
  char *buf = mmap(0, (size_t)INT_MAX + 4096, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE, -1, 0);
  if (buf == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  int null = open("/dev/null", O_WRONLY);
  char path[] = "/tmp/rw-limit-XXXXXX";
  int empty = mkstemp(path);
  if (null < 0 || empty < 0) {
    perror("open");
    return 1;
  }
  unlink(path);
  for (int i = 0; i < 2; i++) {
    errno = 0;
    report("pwrite(/dev/null)", lens[i], pwrite(null, buf, lens[i], 0));
    errno = 0;
    report("pread(empty file)", lens[i], pread(empty, buf, lens[i], 0));
  }
  return 0;
}

// The Linux steps of server.rs chmod_at: open O_PATH|O_NOFOLLOW, fstat, refuse a symlink,
// chmod through /proc/self/fd. Checks a regular file changes, a symlink to a file outside
// leaves that file alone, and a mode-000 file (unreadable to its owner) still changes.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int chmod_at(int dir, const char *name, mode_t mode) {
  int fd = openat(dir, name, O_PATH | O_NOFOLLOW | O_CLOEXEC);
  if (fd < 0) return -errno;
  struct stat st;
  if (fstat(fd, &st)) { int e = errno; close(fd); return -e; }
  if (S_ISLNK(st.st_mode)) { close(fd); return -EOPNOTSUPP; }
  char link[64];
  snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
  int rc = fchmodat(AT_FDCWD, link, mode, 0) ? -errno : 0;
  close(fd);
  return rc;
}

static unsigned mode_of(const char *p) {
  struct stat st;
  return stat(p, &st) ? 0 : st.st_mode & 07777;
}

int main(void) {
  if (chdir("/tmp")) return 2;
  mkdir("share", 0755);
  int fd = open("outside", O_CREAT | O_WRONLY | O_TRUNC, 0600);
  close(fd);
  chmod("outside", 0600);
  int d = open("share", O_RDONLY | O_DIRECTORY);
  unlinkat(d, "link", 0);
  unlinkat(d, "file", 0);
  symlinkat("../outside", d, "link");
  fd = openat(d, "file", O_CREAT | O_WRONLY, 0644);
  close(fd);
  int rc = chmod_at(d, "link", 0777);
  printf("symlink: rc=%d (EOPNOTSUPP is %d); outside's mode %o\n", rc, -EOPNOTSUPP, mode_of("outside"));
  rc = chmod_at(d, "file", 0);
  printf("regular file to 000: rc=%d; mode %o\n", rc, mode_of("share/file"));
  rc = chmod_at(d, "file", 0640);
  printf("mode-000 file to 640: rc=%d; mode %o\n", rc, mode_of("share/file"));
  return 0;
}

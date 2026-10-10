// Linux: the openat2(2) server.rs's open_beneath makes, RESOLVE_BENEATH and
// RESOLVE_NO_SYMLINKS: a directory's path opens; a symlink anywhere in it, even to the same
// directory, is ELOOP; a way out of the starting directory is EXDEV. Run in an empty DIR.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/openat2.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int beneath(int dir, const char *path) {
  struct open_how how = {
      .flags = O_RDONLY | O_DIRECTORY | O_CLOEXEC,
      .resolve = RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
  };
  int fd = syscall(SYS_openat2, dir, path, &how, sizeof how);
  int e = fd < 0 ? errno : 0;
  if (fd >= 0) close(fd);
  return e;
}

int main(int argc, char **argv) {
  if (argc < 2 || chdir(argv[1])) return 2;
  mkdir("share", 0755);
  mkdir("share/a", 0755);
  mkdir("share/a/b", 0755);
  mkdir("outside", 0755);
  int dir = open("share", O_RDONLY | O_DIRECTORY);
  printf("a/b: %s\n", strerror(beneath(dir, "a/b")));
  rename("share/a", "share/real");
  symlink("real", "share/a");
  printf("a/b through a symlink to itself: %s (ELOOP is %s)\n", strerror(beneath(dir, "a/b")),
         strerror(ELOOP));
  unlink("share/a");
  symlink("../outside", "share/a");
  printf("a through a symlink out: %s\n", strerror(beneath(dir, "a")));
  printf("../outside: %s (EXDEV is %s)\n", strerror(beneath(dir, "../outside")), strerror(EXDEV));
  return 0;
}

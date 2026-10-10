#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv) {
  if (argc < 2 || chdir(argv[1])) return 2;
  mkdir("share", 0755);
  int fd = open("outside", O_CREAT | O_WRONLY | O_TRUNC, 0600);
  write(fd, "secret", 6);
  close(fd);
  chmod("outside", 0600);
  int d = open("share", O_RDONLY | O_DIRECTORY);
  unlinkat(d, "evil", 0);
  unlinkat(d, "hard", 0);
  if (symlinkat("../outside", d, "evil")) perror("symlinkat");
  int r = linkat(d, "evil", d, "hard", 0);
  struct stat st;
  fstatat(d, "hard", &st, AT_SYMLINK_NOFOLLOW);
  printf("linkat flag 0: rc=%d errno=%s; 'hard' is %s\n", r, r ? strerror(errno) : "-",
         S_ISLNK(st.st_mode) ? "a symlink (not followed)" : "a hard link to the target (FOLLOWED)");
  r = fchmodat(d, "evil", 0666, 0);
  stat("outside", &st);
  printf("fchmodat flag 0 on a symlink: rc=%d; outside's mode now %o\n", r, st.st_mode & 07777);
  chmod("outside", 0600);
  r = fchmodat(d, "evil", 0666, AT_SYMLINK_NOFOLLOW);
  stat("outside", &st);
  printf("fchmodat AT_SYMLINK_NOFOLLOW on a symlink: rc=%d errno=%s; outside's mode now %o\n", r,
         r ? strerror(errno) : "-", st.st_mode & 07777);
  return 0;
}

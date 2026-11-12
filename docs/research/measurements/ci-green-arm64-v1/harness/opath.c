#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>
int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    int fd = open(dir, O_RDONLY | O_PATH | O_CLOEXEC);
    if (fd < 0) { printf("open: %s\n", strerror(errno)); return 1; }
    int fl = fcntl(fd, F_GETFL);
    printf("F_GETFL=%#o (O_PATH set: %d)\n", fl, (fl & O_PATH) != 0);
    errno = 0;
    int r = fcntl(fd, F_SETFL, fl | O_NONBLOCK);
    printf("libc fcntl F_SETFL -> %d errno=%d (%s)\n", r, errno, strerror(errno));
    errno = 0;
    long s = syscall(SYS_fcntl, fd, F_SETFL, (long)(fl | O_NONBLOCK));
    printf("raw SYS_fcntl F_SETFL -> %ld errno=%d (%s)\n", s, errno, strerror(errno));
    int fl2 = fcntl(fd, F_GETFL);
    printf("F_GETFL after=%#o\n", fl2);
    return 0;
}

// How many bytes a FIFO and a pipe take before a write would wait, no reader reading
// (PM M261): a slot's answer is a FIFO, and a client's stderr often a pipe.
//
//   cc -O2 -o capacity capacity.c && ./capacity DIR
//
// Writes to each, its end not blocking, until EAGAIN: STEP bytes at a time, then halves of
// that, to the byte. A write of more than PIPE_BUF may be taken in part (pipe(7)), and a
// large write may find more room than small ones (macOS grows a pipe's buffer for one).
//
//   ./capacity DIR [STEP]    (STEP: 1024 by default)
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static char buf[1 << 20];

static long fill(int fd, size_t first) {
    memset(buf, 'x', sizeof buf);
    long total = 0;
    for (size_t step = first; step >= 1; step /= 2) {
        for (;;) {
            ssize_t n = write(fd, buf, step);
            if (n > 0) {
                total += n;
                continue;
            }
            if (n < 0 && errno == EAGAIN) break;
            perror("write");
            return -1;
        }
        if (step == 1) break;
    }
    return total;
}

int main(int argc, char **argv) {
    if (argc != 2 && argc != 3) {
        fprintf(stderr, "usage: %s DIR [STEP]\n", argv[0]);
        return 2;
    }
    size_t first = argc == 3 ? (size_t)atol(argv[2]) : 1024;
    if (first < 1 || first > sizeof buf) {
        fprintf(stderr, "STEP: 1 to %zu\n", sizeof buf);
        return 2;
    }
    char path[PATH_MAX];
    snprintf(path, sizeof path, "%s/fifo", argv[1]);
    unlink(path);
    if (mkfifo(path, 0600) != 0) {
        perror("mkfifo");
        return 1;
    }
    int r = open(path, O_RDONLY | O_NONBLOCK);
    int w = open(path, O_WRONLY | O_NONBLOCK);
    if (r < 0 || w < 0) {
        perror("open");
        return 1;
    }
    printf("fifo, %zu at a time: %ld bytes\n", first, fill(w, first));
    int p[2];
    if (pipe(p) != 0) {
        perror("pipe");
        return 1;
    }
    fcntl(p[1], F_SETFL, O_NONBLOCK);
    printf("pipe, %zu at a time: %ld bytes\n", first, fill(p[1], first));
    printf("PIPE_BUF: %d\n", PIPE_BUF);
    unlink(path);
    return 0;
}

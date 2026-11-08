/* What a copy from memory that may fault costs a VMM's thread, against memcpy (PM M304):
 * through a pipe, the source written into it and read back out (POSIX: write(2) and
 * read(2) answer an inaccessible buffer with EFAULT, never a signal), and on Linux by
 * process_vm_readv(2) of the process itself. Each copy of LEN bytes from a file's shared
 * mapping, all of whose pages are resident, into a heap buffer: n copies a size, timed
 * one by one. Then each method is given a page past its file's end, which it must answer
 * with an error.
 * Usage: copy FILE N  (FILE at least 1 MiB) */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

static int pfd[2];

static uint64_t now(void) {
#ifdef __APPLE__
    /* CLOCK_MONOTONIC's are whole microseconds on macOS. */
    return clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
#else
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
#endif
}

/* 0, or -1 with errno where a byte of src could not be read. */
static int pipe_copy(void *dst, const void *src, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t w = write(pfd[1], (const char *)src + done, len - done);
        if (w < 0) return -1;
        size_t got = 0;
        while (got < (size_t)w) {
            ssize_t r = read(pfd[0], (char *)dst + done + got, (size_t)w - got);
            if (r <= 0) return -1;
            got += (size_t)r;
        }
        done += (size_t)w;
    }
    return 0;
}

#ifdef __linux__
static int pvm_copy(void *dst, const void *src, size_t len) {
    struct iovec l = {dst, len}, r = {(void *)src, len};
    ssize_t n = process_vm_readv(getpid(), &l, 1, &r, 1, 0);
    return n == (ssize_t)len ? 0 : -1;
}
#endif

static int mem_copy(void *dst, const void *src, size_t len) {
    memcpy(dst, src, len);
    __asm__ volatile("" ::: "memory");
    return 0;
}

static int cmp(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void report(const char *name, size_t len, uint64_t *t, int n) {
    qsort(t, (size_t)n, sizeof *t, cmp);
    printf("%-8s %8zu B  n=%d  p50 %.2f  p90 %.2f  p99 %.2f  max %.2f us\n", name, len, n,
           t[n / 2] / 1e3, t[n * 9 / 10] / 1e3, t[n * 99 / 100] / 1e3, t[n - 1] / 1e3);
}

int main(int argc, char **argv) {
    if (argc != 3) return 2;
    int n = atoi(argv[2]);
    int fd = open(argv[1], O_RDWR);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_size < (1 << 20) || n < 1) return 2;
    size_t flen = 1 << 20;
    char *map = mmap(NULL, flen, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) return 3;
    volatile char sink = 0;
    for (size_t i = 0; i < flen; i += 4096) sink += map[i];
    if (pipe(pfd) != 0) return 4;
    fcntl(pfd[0], F_SETFL, O_NONBLOCK);
    fcntl(pfd[1], F_SETFL, O_NONBLOCK);
    char *buf = malloc(flen);
    uint64_t *t = malloc(sizeof *t * (size_t)n);
    if (!buf || !t) return 5;
    memset(buf, 1, flen);
    struct { const char *name; int (*f)(void *, const void *, size_t); } ms[] = {
        {"memcpy", mem_copy}, {"pipe", pipe_copy},
#ifdef __linux__
        {"pvm", pvm_copy},
#endif
    };
    size_t sizes[] = {4096, 65536, 1 << 20};
    for (size_t s = 0; s < sizeof sizes / sizeof *sizes; s++)
        for (size_t m = 0; m < sizeof ms / sizeof *ms; m++) {
            for (int i = 0; i < n; i++) {
                uint64_t a = now();
                if (ms[m].f(buf, map, sizes[s]) != 0) { perror(ms[m].name); return 6; }
                t[i] = now() - a;
            }
            report(ms[m].name, sizes[s], t, n);
        }
    /* A page past the file's end: the file cut to 64 KiB under its 1 MiB mapping. */
    if (ftruncate(fd, 65536) != 0) return 7;
    for (size_t m = 1; m < sizeof ms / sizeof *ms; m++) {
        errno = 0;
        int r = ms[m].f(buf, map + (512 << 10), 4096);
        printf("%-8s past the end: %d errno %d (%s)\n", ms[m].name, r, errno, strerror(errno));
    }
    return 0;
}

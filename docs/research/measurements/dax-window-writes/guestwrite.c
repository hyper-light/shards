// What a guest's 4 KiB write at a random page of a file on a volume costs it (D189, PM
// M283's question asked in the guest): pwrite(2) with O_DIRECT, as fio's `direct=1` writes.
// Through a volume without a DAX window, each is a FUSE_WRITE its host's server answers
// with a pwrite(2); through one with a window, it is the guest's stores into the window's
// mapping of the file (fs/fuse/dax.c, fuse_dax_write_iter), whose first store to a page its
// host does not hold waits for the host to read the page in. The file is the host's, laid
// out whole beforehand, its pages in the host's page cache or not as the host left them.
// N writes of SIZE bytes (4096 unless given, a multiple of it), each at a SIZE-aligned
// offset drawn from a fixed seed, so that each run writes the same places; each write's
// time on CLOCK_MONOTONIC. With `premap`, a page of each 2 MiB range (FUSE_DAX_SZ) is
// read first, untimed: through a window, so that each range is mapped before its writes
// come, which leaves them the stores alone (a mapping to be read, which a range's first
// store upgrades). With `premapw`, a page of each is written first instead, so that each
// range is mapped to be written before its writes come.
//
// Usage: guestwrite PATH N [SIZE [premap|premapw]]
//   prints: n p50_us p90_us p99_us max_us mib_s
//   and on stderr, the same of the writes that were their 2 MiB range's first, and of the
//   rest: `first n p50 p90 p99 max; later n p50 p90 p99 max`.
#define _GNU_SOURCE
#ifndef __linux__
#error "guestwrite runs in a Linux guest: zig cc -target <arch>-linux-musl -static"
#endif
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define PAGE 4096
#define RANGE (2u << 20)

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

static uint64_t next(uint64_t *s) {
    // xorshift64*
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    return *s * 2685821657736338717ull;
}

static int cmp(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void die(const char *what) {
    fprintf(stderr, "guestwrite: %s: %s\n", what, strerror(errno));
    exit(1);
}

// The p-th percentile of n sorted values, nearest rank.
static uint64_t pct(const uint64_t *v, size_t n, double p) {
    size_t i = (size_t)(p / 100.0 * (double)n);
    return v[i < n ? i : n - 1];
}

int main(int argc, char **argv) {
    if (argc < 3 || argc > 5 ||
        (argc == 5 && strcmp(argv[4], "premap") != 0 && strcmp(argv[4], "premapw") != 0)) {
        fprintf(stderr, "usage: guestwrite PATH N [SIZE [premap|premapw]]\n");
        return 2;
    }
    size_t n = strtoul(argv[2], NULL, 10);
    size_t size = argc >= 4 ? strtoul(argv[3], NULL, 10) : PAGE;
    if (size == 0 || size % PAGE != 0) {
        fprintf(stderr, "guestwrite: SIZE is a multiple of %d\n", PAGE);
        return 2;
    }
    int fd = open(argv[1], O_RDWR | O_DIRECT);
    if (fd < 0)
        die("open");
    struct stat st;
    if (fstat(fd, &st) != 0)
        die("fstat");
    uint64_t places = (uint64_t)st.st_size / size;
    if (places == 0 || n == 0) {
        fprintf(stderr, "guestwrite: no place to write\n");
        return 2;
    }
    void *buf;
    if (posix_memalign(&buf, PAGE, size) != 0)
        die("posix_memalign");
    memset(buf, 0x5a, size);
    if (argc == 5 && strcmp(argv[4], "premap") == 0)
        for (off_t at = 0; at + PAGE <= st.st_size; at += RANGE)
            if (pread(fd, buf, PAGE, at) != PAGE)
                die("pread");
    if (argc == 5 && strcmp(argv[4], "premapw") == 0)
        for (off_t at = 0; at + PAGE <= st.st_size; at += RANGE)
            if (pwrite(fd, buf, PAGE, at) != PAGE)
                die("pwrite");
    uint64_t *took = calloc(n, sizeof *took);
    uint64_t *first = calloc(n, sizeof *first), *later = calloc(n, sizeof *later);
    size_t ranges = (size_t)((st.st_size + RANGE - 1) / RANGE);
    unsigned char *met = calloc(ranges ? ranges : 1, 1);
    if (!took || !first || !later || !met)
        die("calloc");
    size_t firsts = 0, laters = 0;
    uint64_t seed = 0x9e3779b97f4a7c15ull, all = now_ns();
    for (size_t i = 0; i < n; i++) {
        off_t at = (off_t)(next(&seed) % places) * (off_t)size;
        uint64_t t = now_ns();
        if (pwrite(fd, buf, size, at) != (ssize_t)size)
            die("pwrite");
        took[i] = now_ns() - t;
        // Its range's first write, or a later one (a write meeting two ranges counts by its
        // first).
        size_t r = (size_t)(at / RANGE);
        if (!met[r]) {
            met[r] = 1;
            first[firsts++] = took[i];
        } else {
            later[laters++] = took[i];
        }
    }
    all = now_ns() - all;
    close(fd);
    qsort(took, n, sizeof *took, cmp);
    printf("%zu %.1f %.1f %.1f %.1f %.1f\n", n, pct(took, n, 50) / 1e3, pct(took, n, 90) / 1e3,
           pct(took, n, 99) / 1e3, took[n - 1] / 1e3, (double)n * size / (1 << 20) / (all / 1e9));
    const char *names[2] = {"first", "later"};
    uint64_t *sets[2] = {first, later};
    size_t counts[2] = {firsts, laters};
    for (int k = 0; k < 2; k++) {
        size_t c = counts[k];
        qsort(sets[k], c, sizeof *sets[k], cmp);
        if (c == 0)
            fprintf(stderr, "%s 0%s", names[k], k == 0 ? "; " : "\n");
        else
            fprintf(stderr, "%s %zu %.1f %.1f %.1f %.1f%s", names[k], c, pct(sets[k], c, 50) / 1e3,
                    pct(sets[k], c, 90) / 1e3, pct(sets[k], c, 99) / 1e3, sets[k][c - 1] / 1e3,
                    k == 0 ? "; " : "\n");
    }
    return 0;
}

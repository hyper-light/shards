// What a 4 KiB write at a random page of a file costs its host, through a shared mapping
// of the file (as a guest's write through a virtio-fs DAX window reaches it: the window
// maps the file's range MAP_SHARED, and the guest's first store to a page faults it in
// and makes it writable) or by pwrite(2) (as a FUSE_WRITE reaches the server without a
// window). `map-willneed` maps as `map` does, and asks the kernel to read each 2 MiB range
// ahead (POSIX_FADV_WILLNEED; F_RDADVISE on macOS) as the first write into it comes, as a
// window's server could as it maps the range (FUSE_DAX_SZ). The file, SIZE MiB, is written
// whole and synced first, as fio lays out its file; `cold` then has the kernel drop its
// pages (POSIX_FADV_DONTNEED on Linux; on macOS the file is written with F_NOCACHE set,
// which keeps it out of the unified buffer cache), `warm` keeps them. N writes, each at a
// page drawn from a fixed seed, the same pages in each mode; each write's time on
// CLOCK_MONOTONIC, an advice's with its range's first write.
//
// Usage: mkwrite PATH SIZE_MIB N map|map-willneed|pwrite warm|cold
//   prints: mode cache n p50_us p90_us p99_us max_us mib_s
//
// The file is the caller's to remove.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
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
    fprintf(stderr, "mkwrite: %s: %s\n", what, strerror(errno));
    exit(1);
}

int main(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr, "usage: mkwrite PATH SIZE_MIB N map|map-willneed|pwrite warm|cold\n");
        return 2;
    }
    const char *path = argv[1];
    size_t size = (size_t)strtoull(argv[2], NULL, 10) << 20;
    size_t n = (size_t)strtoull(argv[3], NULL, 10);
    int willneed = strcmp(argv[4], "map-willneed") == 0;
    int map = willneed || strcmp(argv[4], "map") == 0;
    int cold = strcmp(argv[5], "cold") == 0;
    size_t pages = size / PAGE;
    if (pages == 0 || n == 0) {
        fprintf(stderr, "mkwrite: SIZE_MIB and N must be positive\n");
        return 2;
    }

    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) die("open");
#ifdef __APPLE__
    if (cold && fcntl(fd, F_NOCACHE, 1) != 0) die("F_NOCACHE");
#endif
    // Laid out whole: every block allocated and written, as fio's layout leaves it.
    char *chunk = malloc(1 << 20);
    if (!chunk) die("malloc");
    memset(chunk, 0x5a, 1 << 20);
    for (size_t at = 0; at < size; at += 1 << 20) {
        if (pwrite(fd, chunk, 1 << 20, (off_t)at) != 1 << 20) die("layout");
    }
    if (fsync(fd) != 0) die("fsync");
#ifdef __linux__
    if (cold && posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED) != 0) die("fadvise");
#endif
#ifdef __APPLE__
    if (cold) {
        if (fcntl(fd, F_NOCACHE, 0) != 0) die("F_NOCACHE off");
    }
#endif

    char *base = NULL;
    if (map) {
        base = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (base == MAP_FAILED) die("mmap");
    }
    char page[PAGE];
    memset(page, 0xa5, sizeof page);
    uint64_t *took = malloc(n * sizeof *took);
    if (!took) die("malloc");
    // The ranges advised so far.
    unsigned char *advised = calloc(size / RANGE + 1, 1);
    if (!advised) die("calloc");
    uint64_t seed = 0x5eed;
    uint64_t start = now_ns();
    for (size_t i = 0; i < n; i++) {
        size_t p = (size_t)(next(&seed) % pages);
        page[0] = (char)i;
        uint64_t t = now_ns();
        size_t r = p * PAGE / RANGE;
        if (willneed && !advised[r]) {
            advised[r] = 1;
#ifdef __linux__
            if (posix_fadvise(fd, (off_t)(r * RANGE), RANGE, POSIX_FADV_WILLNEED) != 0) die("WILLNEED");
#endif
#ifdef __APPLE__
            struct radvisory ahead = {.ra_offset = (off_t)(r * RANGE), .ra_count = (int)RANGE};
            if (fcntl(fd, F_RDADVISE, &ahead) != 0) die("F_RDADVISE");
#endif
        }
        if (map) {
            memcpy(base + p * PAGE, page, PAGE);
        } else if (pwrite(fd, page, PAGE, (off_t)(p * PAGE)) != PAGE) {
            die("pwrite");
        }
        took[i] = now_ns() - t;
    }
    uint64_t all = now_ns() - start;
    qsort(took, n, sizeof *took, cmp);
    double mib = (double)n * PAGE / (1 << 20);
    printf("%s %s %zu %.1f %.1f %.1f %.1f %.1f\n", argv[4], cold ? "cold" : "warm", n,
           took[n / 2] / 1e3, took[n * 9 / 10] / 1e3, took[n * 99 / 100] / 1e3, took[n - 1] / 1e3,
           mib / (all / 1e9));
    if (map && munmap(base, size) != 0) die("munmap");
    close(fd);
    return 0;
}

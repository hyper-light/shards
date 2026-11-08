// What a guest's reads and writes of a large file cost it through a volume's DAX window,
// its ranges on 4 KiB pages or on 2 MiB folios (D189's Next, PM M308). Over FILE, on
// CLOCK_MONOTONIC:
//   seq FILE            read(2) it whole, a MiB a call: its seconds
//   touch FILE N SEED   N distinct ranges of the guest's 2 MiB (FUSE_DAX_SZ), drawn from
//                       SEED: a 4 KiB pread(2) at a random page of each, in the order drawn
//                       (`first`: each range's first touch, which maps it), then one at
//                       another page of each (`second`: a range mapped, a page not yet
//                       touched)
//   write FILE N SEED   N distinct ranges drawn from SEED: a 4 KiB pwrite(2) of new bytes at
//                       a random page of each (`write`), then fsync(2) of FILE, which the
//                       host's writeback of what they dirtied waits for. A whole page its host
//                       does not hold goes as a FUSE_WRITE (D189, PM M301), whatever the
//                       window's pages are
//   rewrite FILE N SEED as write, each range's page read first (untimed) and another page of
//                       it written (`rewrite`): what the read brought into the host's page
//                       cache is written through the window, a store into its folio
//   partial FILE N SEED as write, 512 bytes at a random 512-byte offset of each page
//                       (`partial`): a part of a page goes through the window, its host
//                       reading the page first
// then, untimed, whether statx(2) says FILE is DAX, and the guest's Cached and MemFree
// (/proc/meminfo) before and after.
//
// Usage: guestrand seq FILE | touch|write|rewrite|partial FILE N SEED
//   prints: seq bytes B s S
//       or: first n N p50 P p90 P p99 P max M, then second n ..., in µs
//       or: write (rewrite, partial) n N p50 ... in µs, then fsync s S
//   then:   dax 1|0
//   then:   meminfo cached_kib BEFORE AFTER memfree_kib BEFORE AFTER
#define _GNU_SOURCE
#ifndef __linux__
#error "guestrand runs in a Linux guest: zig cc -target <arch>-linux-musl -static"
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

// statx(2)'s, as <linux/stat.h> numbers them (musl's own definitions may lag the kernel's).
#ifndef STATX_ATTR_DAX
#define STATX_ATTR_DAX 0x00200000
#endif
#ifndef STATX_BASIC_STATS
#define STATX_BASIC_STATS 0x000007ffU
#endif

// The guest's DAX range (fs/fuse/dax.c FUSE_DAX_SZ) and a page.
#define RANGE (2u << 20)
#define PAGE 4096u
#define CHUNK (1u << 20)

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

static void die(const char *what, const char *name) {
    fprintf(stderr, "guestrand: %s%s%s: %s\n", what, name ? " " : "", name ? name : "",
            strerror(errno));
    exit(1);
}

// The p-th percentile of n sorted values, nearest rank.
static uint64_t pct(const uint64_t *v, size_t n, double p) {
    size_t i = (size_t)(p / 100.0 * (double)n);
    return v[i < n ? i : n - 1];
}

static void report(const char *what, uint64_t *took, size_t n) {
    qsort(took, n, sizeof *took, cmp);
    printf("%s n %zu p50 %.1f p90 %.1f p99 %.1f max %.1f\n", what, n, pct(took, n, 50) / 1e3,
           pct(took, n, 90) / 1e3, pct(took, n, 99) / 1e3, took[n - 1] / 1e3);
    fflush(stdout);
}

// /proc/meminfo's KEY, in KiB; -1 where it says none.
static long meminfo(const char *key) {
    FILE *f = fopen("/proc/meminfo", "r");
    if (!f)
        return -1;
    char line[256];
    size_t k = strlen(key);
    long v = -1;
    while (fgets(line, sizeof line, f))
        if (strncmp(line, key, k) == 0 && line[k] == ':') {
            v = strtol(line + k + 1, NULL, 10);
            break;
        }
    fclose(f);
    return v;
}

// `len` bytes at `at`, whole, read or written.
static void page_io(int fd, char *buf, off_t at, size_t len, int write, const char *name) {
    size_t done = 0;
    while (done < len) {
        ssize_t r = write ? pwrite(fd, buf + done, len - done, at + (off_t)done)
                          : pread(fd, buf + done, len - done, at + (off_t)done);
        if (r < 0) {
            if (errno == EINTR)
                continue;
            die(write ? "pwrite" : "pread", name);
        }
        if (r == 0) {
            errno = EIO;
            die("short page at the file's end", name);
        }
        done += (size_t)r;
    }
}

int main(int argc, char **argv) {
    int seq = argc == 3 && strcmp(argv[1], "seq") == 0;
    int touch = argc == 5 && strcmp(argv[1], "touch") == 0;
    int rewrite = argc == 5 && strcmp(argv[1], "rewrite") == 0;
    int partial = argc == 5 && strcmp(argv[1], "partial") == 0;
    int write = (argc == 5 && strcmp(argv[1], "write") == 0) || rewrite || partial;
    if (!seq && !touch && !write) {
        fprintf(stderr, "usage: guestrand seq FILE | touch|write|rewrite|partial FILE N SEED\n");
        return 2;
    }
    const char *name = argv[2];
    int fd = open(name, write ? O_RDWR : O_RDONLY);
    if (fd < 0)
        die("open", name);
    struct stat st;
    if (fstat(fd, &st) != 0)
        die("fstat", name);
    char *buf = malloc(CHUNK);
    if (!buf)
        die("malloc", NULL);
    long cached0 = meminfo("Cached"), free0 = meminfo("MemFree");
    if (seq) {
        uint64_t t = now_ns(), total = 0;
        for (;;) {
            ssize_t r = read(fd, buf, CHUNK);
            if (r < 0) {
                if (errno == EINTR)
                    continue;
                die("read", name);
            }
            if (r == 0)
                break;
            total += (uint64_t)r;
        }
        printf("seq bytes %llu s %.3f\n", (unsigned long long)total, (now_ns() - t) / 1e9);
    } else {
        char *end;
        unsigned long long n = strtoull(argv[3], &end, 10);
        uint64_t seed = strtoull(argv[4], NULL, 10) | 1;
        uint64_t ranges = (uint64_t)st.st_size / RANGE;
        if (*end || n == 0 || n > ranges) {
            fprintf(stderr, "guestrand: N must be 1 to %llu, the whole ranges of %s\n",
                    (unsigned long long)ranges, name);
            return 2;
        }
        // N distinct ranges: the first N of all of them shuffled from SEED (Fisher-Yates),
        // and a page of each, then another page of each for `second`.
        uint64_t *order = malloc(ranges * sizeof *order);
        uint64_t *took = calloc(n, sizeof *took);
        uint32_t *page = calloc(n, sizeof *page);
        if (!order || !took || !page)
            die("malloc", NULL);
        for (uint64_t i = 0; i < ranges; i++)
            order[i] = i;
        for (uint64_t i = ranges - 1; i > 0; i--) {
            uint64_t j = next(&seed) % (i + 1), t = order[i];
            order[i] = order[j];
            order[j] = t;
        }
        for (size_t i = 0; i < n; i++)
            page[i] = (uint32_t)(next(&seed) % (RANGE / PAGE));
        for (size_t i = 0; i < n; i++) {
            off_t at = (off_t)(order[i] * RANGE + (uint64_t)page[i] * PAGE);
            size_t len = PAGE;
            if (rewrite) {
                // The range's page read, then another of its pages written.
                page_io(fd, buf, at, PAGE, 0, name);
                uint32_t other = (uint32_t)(next(&seed) % (RANGE / PAGE - 1));
                other += other >= page[i];
                at = (off_t)(order[i] * RANGE + (uint64_t)other * PAGE);
            } else if (partial) {
                len = 512;
                at += (off_t)(next(&seed) % (PAGE / 512) * 512);
            }
            if (write)
                for (size_t b = 0; b < len; b += 8) {
                    uint64_t v = next(&seed);
                    memcpy(buf + b, &v, 8);
                }
            uint64_t t = now_ns();
            page_io(fd, buf, at, len, write, name);
            took[i] = now_ns() - t;
        }
        report(!write ? "first" : rewrite ? "rewrite" : partial ? "partial" : "write", took, n);
        if (write) {
            uint64_t t = now_ns();
            if (fsync(fd) != 0)
                die("fsync", name);
            printf("fsync s %.3f\n", (now_ns() - t) / 1e9);
        } else {
            for (size_t i = 0; i < n; i++) {
                // Another page of the same range: one of the other 511, drawn from SEED.
                uint32_t other = (uint32_t)(next(&seed) % (RANGE / PAGE - 1));
                other += other >= page[i];
                off_t at = (off_t)(order[i] * RANGE + (uint64_t)other * PAGE);
                uint64_t t = now_ns();
                page_io(fd, buf, at, PAGE, 0, name);
                took[i] = now_ns() - t;
            }
            report("second", took, n);
        }
    }
    long cached1 = meminfo("Cached"), free1 = meminfo("MemFree");
    struct statx sx;
    if (statx(AT_FDCWD, name, 0, STATX_BASIC_STATS, &sx) != 0)
        die("statx", name);
    printf("dax %d\n", (sx.stx_attributes & STATX_ATTR_DAX) != 0);
    printf("meminfo cached_kib %ld %ld memfree_kib %ld %ld\n", cached0, cached1, free0, free1);
    return 0;
}

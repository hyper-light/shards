// What a guest's small files cost it through a volume's DAX window and through FUSE alone
// (D189: from what size a file is mapped, DAX_SMALLEST). Over the N regular files of DIR,
// all of one size, in an order drawn from a fixed seed, each file's time on CLOCK_MONOTONIC
// in each PHASE named, in the order named:
//   first      open(2), read(2) it whole, close(2): its lookup, and through a window its
//              FUSE_SETUPMAPPING, as its first read pays them
//   again      the same once more: FUSE drops a file's page cache at each open (no
//              FOPEN_KEEP_CACHE), a window keeps its mapping
//   overwrite  open(2) O_RDWR, pwrite(2) of its whole size at 0, close(2): the one write
//              of a small file a window takes, its size unchanged
//   rewrite    open(2) O_WRONLY|O_TRUNC, write(2) of its size, close(2): through FUSE
//              either way, its size dropped to 0 first
//   stat       fstatat(2) of it
// then, untimed, statx(2) of each, how many say STATX_ATTR_DAX (taken last, so that no
// phase finds its lookups made), and the guest's Cached and MemFree (/proc/meminfo) before
// the first phase and after the last.
//
// Usage: guestsmall DIR PHASE...
//   prints a line a phase: PHASE n p50_us p90_us p99_us max_us
//   then: dax K of N
//   then: meminfo cached_kib BEFORE AFTER memfree_kib BEFORE AFTER
#define _GNU_SOURCE
#ifndef __linux__
#error "guestsmall runs in a Linux guest: zig cc -target <arch>-linux-musl -static"
#endif
#include <dirent.h>
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

#define BIGGEST (2u << 20)

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

static int by_name(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

static void die(const char *what, const char *name) {
    fprintf(stderr, "guestsmall: %s%s%s: %s\n", what, name ? " " : "", name ? name : "",
            strerror(errno));
    exit(1);
}

// The p-th percentile of n sorted values, nearest rank.
static uint64_t pct(const uint64_t *v, size_t n, double p) {
    size_t i = (size_t)(p / 100.0 * (double)n);
    return v[i < n ? i : n - 1];
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

// Reads all of `fd` into buf, at most cap bytes: how many.
static ssize_t read_whole(int fd, char *buf, size_t cap, const char *name) {
    size_t got = 0;
    for (;;) {
        ssize_t r = read(fd, buf + got, cap - got);
        if (r < 0) {
            if (errno == EINTR)
                continue;
            die("read", name);
        }
        if (r == 0 || (got += (size_t)r) == cap)
            return (ssize_t)got;
    }
}

static void write_whole(int fd, const char *buf, size_t n, int at_zero, const char *name) {
    size_t done = 0;
    while (done < n) {
        ssize_t w = at_zero ? pwrite(fd, buf + done, n - done, (off_t)done)
                            : write(fd, buf + done, n - done);
        if (w < 0) {
            if (errno == EINTR)
                continue;
            die(at_zero ? "pwrite" : "write", name);
        }
        done += (size_t)w;
    }
}

int main(int argc, char **argv) {
    static const char *phases[] = {"first", "again", "overwrite", "rewrite", "stat"};
    if (argc < 3) {
        fprintf(stderr, "usage: guestsmall DIR PHASE...   (PHASE: first again overwrite rewrite stat)\n");
        return 2;
    }
    for (int i = 2; i < argc; i++) {
        int known = 0;
        for (size_t p = 0; p < sizeof phases / sizeof *phases; p++)
            known |= strcmp(argv[i], phases[p]) == 0;
        if (!known) {
            fprintf(stderr, "guestsmall: no phase %s\n", argv[i]);
            return 2;
        }
    }
    int dir = open(argv[1], O_RDONLY | O_DIRECTORY);
    if (dir < 0)
        die("open", argv[1]);
    // The files' names, read whole before any is timed, sorted, then shuffled from a fixed
    // seed (Fisher-Yates), so that each run takes them in the same order, not the order the
    // directory lists them in.
    DIR *d = fdopendir(dup(dir));
    if (!d)
        die("fdopendir", argv[1]);
    size_t n = 0, cap = 1024;
    char **names = malloc(cap * sizeof *names);
    if (!names)
        die("malloc", NULL);
    for (struct dirent *e; (e = readdir(d));) {
        if (e->d_type != DT_REG)
            continue;
        if (n == cap) {
            cap *= 2;
            char **more = realloc(names, cap * sizeof *names);
            if (!more)
                die("realloc", NULL);
            names = more;
        }
        if (!(names[n++] = strdup(e->d_name)))
            die("strdup", NULL);
    }
    closedir(d);
    if (n == 0) {
        fprintf(stderr, "guestsmall: no regular file in %s\n", argv[1]);
        return 2;
    }
    qsort(names, n, sizeof *names, by_name);
    uint64_t seed = 0x9e3779b97f4a7c15ull;
    for (size_t i = n - 1; i > 0; i--) {
        size_t j = (size_t)(next(&seed) % (i + 1));
        char *t = names[i];
        names[i] = names[j];
        names[j] = t;
    }
    char *buf = malloc(BIGGEST);
    uint64_t *took = calloc(n, sizeof *took);
    // Each file's size, as its reads find it; or, where a write comes before any read, as
    // an untimed fstatat(2) says.
    size_t *sizes = calloc(n, sizeof *sizes);
    int sized = 0;
    if (!buf || !took || !sizes)
        die("malloc", NULL);
    memset(buf, 0x5a, BIGGEST);
    long cached0 = meminfo("Cached"), free0 = meminfo("MemFree");
    for (int p = 2; p < argc; p++) {
        const char *phase = argv[p];
        int reads = strcmp(phase, "first") == 0 || strcmp(phase, "again") == 0;
        int writes = strcmp(phase, "overwrite") == 0 || strcmp(phase, "rewrite") == 0;
        if (writes && !sized) {
            for (size_t i = 0; i < n; i++) {
                struct stat st;
                if (fstatat(dir, names[i], &st, 0) != 0)
                    die("fstatat", names[i]);
                if ((uint64_t)st.st_size > BIGGEST) {
                    fprintf(stderr, "guestsmall: %s is past %u bytes\n", names[i], BIGGEST);
                    return 2;
                }
                sizes[i] = (size_t)st.st_size;
            }
            sized = 1;
        }
        for (size_t i = 0; i < n; i++) {
            const char *name = names[i];
            uint64_t t = now_ns();
            if (reads) {
                int fd = openat(dir, name, O_RDONLY);
                if (fd < 0)
                    die("open", name);
                ssize_t got = read_whole(fd, buf, BIGGEST, name);
                close(fd);
                took[i] = now_ns() - t;
                sizes[i] = (size_t)got;
            } else if (writes) {
                int overwrite = strcmp(phase, "overwrite") == 0;
                int fd = openat(dir, name, overwrite ? O_RDWR : O_WRONLY | O_TRUNC);
                if (fd < 0)
                    die("open", name);
                write_whole(fd, buf, sizes[i], overwrite, name);
                close(fd);
                took[i] = now_ns() - t;
            } else {
                struct stat st;
                if (fstatat(dir, name, &st, 0) != 0)
                    die("fstatat", name);
                took[i] = now_ns() - t;
            }
        }
        if (reads)
            sized = 1;
        qsort(took, n, sizeof *took, cmp);
        printf("%s %zu %.1f %.1f %.1f %.1f\n", phase, n, pct(took, n, 50) / 1e3,
               pct(took, n, 90) / 1e3, pct(took, n, 99) / 1e3, took[n - 1] / 1e3);
        fflush(stdout);
    }
    long cached1 = meminfo("Cached"), free1 = meminfo("MemFree");
    size_t dax = 0;
    for (size_t i = 0; i < n; i++) {
        struct statx sx;
        if (statx(dir, names[i], 0, STATX_BASIC_STATS, &sx) != 0)
            die("statx", names[i]);
        dax += (sx.stx_attributes & STATX_ATTR_DAX) != 0;
    }
    printf("dax %zu of %zu\n", dax, n);
    printf("meminfo cached_kib %ld %ld memfree_kib %ld %ld\n", cached0, cached1, free0, free1);
    return 0;
}
